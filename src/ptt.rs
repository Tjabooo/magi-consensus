//! Push to talk: `start` records in the background and returns; `stop` ends the recording and
//! returns the transcript. `start` while recording behaves as `stop` (a lost key-release event then
//! only means "press again").

use crate::config::{self, Config};
use crate::llm;
use serde_json::json;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn paths() -> (PathBuf, PathBuf) {
    let d = config::runtime_dir();
    (d.join("ptt.json"), d.join("ptt.wav"))
}

fn alive(pid: u32) -> bool {
    // a zombie still has /proc/PID; check its state
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s.rsplit(')').next().map(|r| !r.trim_start().starts_with('Z')).unwrap_or(false),
        Err(_) => false,
    }
}

/// pid of a running recording, if any
pub fn recording() -> Option<u32> {
    let (meta, _) = paths();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(meta).ok()?).ok()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    alive(pid).then_some(pid)
}

pub fn start(cfg: &Config) -> Result<(), String> {
    let (meta, wav) = paths();
    std::fs::create_dir_all(meta.parent().unwrap()).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&wav);
    if cfg.voice.record.is_empty() {
        return Err("voice.record is empty".into());
    }
    let mut args = vec![cfg.voice.max_seconds.to_string()];
    args.extend(cfg.voice.record.iter().cloned());
    args.push(wav.display().to_string());
    let child = Command::new("timeout").args(&args).process_group(0)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().map_err(|e| format!("can't record: {e}"))?;
    std::fs::write(&meta, json!({"pid": child.id()}).to_string()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Stop the recording; Ok(None) = nothing usable was said.
pub fn stop(cfg: &Config) -> Result<Option<String>, String> {
    let (meta, wav) = paths();
    let pid = recording().ok_or("not recording")?;
    let _ = std::fs::remove_file(&meta);
    // SIGINT to the group: timeout + pw-record finish the WAV header
    let _ = Command::new("kill").args(["-INT", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    let t = Instant::now();
    while alive(pid) && t.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let data = std::fs::read(&wav).map_err(|e| format!("no recording: {e}"))?;
    let _ = std::fs::remove_file(&wav);
    let level = rms(&data);
    eprintln!("recorded {:.1} s, level {level:.0} (min_rms {})", data.len().saturating_sub(44) as f64 / 32000.0, cfg.voice.min_rms);
    if level < cfg.voice.min_rms {
        return Ok(None);
    }
    let text = llm::transcribe(&cfg.endpoint, &data, &cfg.voice.language, &cfg.voice.prompt)?;
    let cleaned = text.trim().trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    Ok((!cleaned.is_empty()).then_some(text.trim().to_string()))
}

pub fn cancel() -> bool {
    let (meta, wav) = paths();
    let r = recording();
    if let Some(pid) = r {
        let _ = Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    }
    let _ = std::fs::remove_file(meta);
    let _ = std::fs::remove_file(wav);
    r.is_some()
}

/// RMS of 16-bit little-endian PCM after the 44-byte WAV header (0 if too short).
pub fn rms(wav: &[u8]) -> f64 {
    let pcm = wav.get(44..).unwrap_or(&[]);
    let n = pcm.len() / 2;
    if n < 1600 {
        return 0.0;
    }
    let sum: f64 = pcm.as_chunks::<2>().0.iter().map(|c| {
        let s = i16::from_le_bytes(*c) as f64;
        s * s
    }).sum();
    (sum / n as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_and_tone() {
        let mut w = vec![0u8; 44];
        w.extend(std::iter::repeat_n([0u8, 0u8], 16000).flatten());
        assert_eq!(rms(&w), 0.0);
        let mut t = vec![0u8; 44];
        t.extend(std::iter::repeat_n(1000i16.to_le_bytes(), 16000).flatten());
        assert!((rms(&t) - 1000.0).abs() < 1e-6);
    }
}
