//! Push to talk: `start` records in the background and returns; `stop` ends the recording and
//! returns the transcript. `start` while recording behaves as `stop`.
//!
//! The compositor's key-release bind is not reliable: Hyprland drops a SUPER+ALT+X release bind
//! when a modifier comes up before X. So `start` also launches a watcher (`ptt watch PID`) that reads
//! the hold key straight from the keyboards (evdev, needs the `input` group) and stops the
//! recording itself if no stop came 250 ms after the key went up. It also finishes recordings that
//! ran into `max_seconds`. Whoever claims the recording first (an atomic rename) does the work.

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

fn claimed_path() -> PathBuf {
    config::runtime_dir().join("ptt.claimed.json")
}

fn meta_pid() -> Option<u32> {
    let (meta, _) = paths();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(meta).ok()?).ok()?;
    Some(v.get("pid")?.as_u64()? as u32)
}

/// Take the recording (running or finished) for stopping. Only one caller wins, so the release
/// bind and the watcher never both handle it.
pub fn claim() -> Option<u32> {
    let pid = meta_pid()?;
    std::fs::rename(paths().0, claimed_path()).ok()?;
    Some(pid)
}

pub fn start(cfg: &Config) -> Result<(), String> {
    crate::hush::interrupt(); // the talk key cuts off whatever MAGI is still saying or doing
    let (meta, wav) = paths();
    std::fs::create_dir_all(meta.parent().unwrap()).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&wav);
    if cfg.voice.record.is_empty() {
        return Err("voice.record is empty".into());
    }
    let mut args = vec![cfg.voice.max_seconds.to_string()];
    args.extend(cfg.voice.record.iter().cloned());
    args.push(wav.display().to_string());
    let child = Command::new("timeout")
        .args(&args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't record: {e}"))?;
    std::fs::write(&meta, json!({"pid": child.id()}).to_string()).map_err(|e| e.to_string())?;
    spawn_watcher(child.id());
    Ok(())
}

fn spawn_watcher(pid: u32) {
    let Ok(exe) = std::env::current_exe() else { return };
    let log = config::state_dir().join("ptt.log");
    let _ = std::fs::create_dir_all(config::state_dir());
    let err = std::fs::OpenOptions::new().create(true).append(true).open(log).map(Stdio::from).unwrap_or(Stdio::null());
    let _ = Command::new(exe)
        .args(["ptt", "watch", &pid.to_string()])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .spawn();
}

/// Wait while recording `pid`. true = this watcher has to stop it (the key went up and no stop
/// came, or the recording hit max_seconds); false = someone else stopped or cancelled it.
pub fn watch(cfg: &Config, pid: u32) -> bool {
    let code = key_code(&cfg.voice.hold_key);
    let kbds = code.map(keyboards).unwrap_or_default();
    let held = || code.is_some_and(|c| kbds.iter().any(|f| key_down(f, c)));
    // hold mode only if the key is down right after the press (a toggle from a menu isn't held)
    let t0 = Instant::now();
    let mut hold = false;
    while t0.elapsed() < Duration::from_millis(150) {
        if held() {
            hold = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut up_since: Option<Instant> = None;
    loop {
        if meta_pid() != Some(pid) {
            return false;
        }
        if !alive(pid) {
            eprintln!("{} ptt: recording hit max_seconds ({} s), finishing it", stamp(), cfg.voice.max_seconds);
            return true;
        }
        if hold {
            if held() {
                up_since = None;
            } else if up_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(250) {
                eprintln!(
                    "{} ptt: {} released but no stop came from the compositor, stopping",
                    stamp(),
                    cfg.voice.hold_key
                );
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn stamp() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("[{s}]")
}

/// Stop a claimed recording; Ok(None) = nothing usable was said.
pub fn finish(cfg: &Config, pid: u32) -> Result<Option<llm::Heard>, String> {
    let (_, wav) = paths();
    let _ = std::fs::remove_file(claimed_path());
    // SIGINT to the group: timeout + pw-record finish the WAV header
    if alive(pid) {
        let _ = Command::new("kill").args(["-INT", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    }
    let t = Instant::now();
    while alive(pid) && t.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let data = std::fs::read(&wav).map_err(|e| format!("no recording: {e}"))?;
    let _ = std::fs::remove_file(&wav);
    let level = rms(&data);
    eprintln!(
        "recorded {:.1} s, level {level:.0} (min_rms {})",
        data.len().saturating_sub(44) as f64 / 32000.0,
        cfg.voice.min_rms
    );
    if level < cfg.voice.min_rms {
        return Ok(None);
    }
    let heard =
        llm::transcribe(&cfg.endpoint, &data, &cfg.voice.language, &cfg.voice.prompt, &cfg.voice.language_remap)?;
    let cleaned = heard.text.trim().trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    Ok((!cleaned.is_empty()).then_some(heard))
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

// ── evdev: is the hold key down on any keyboard? ────────────────────────────────────────────

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

const KEY_BYTES: usize = 96; // KEY_MAX 0x2ff → 768 bits

/// _IOR('E', nr, [u8; KEY_BYTES])
fn eviocg(nr: u64) -> u64 {
    (2 << 30) | ((KEY_BYTES as u64) << 16) | (0x45 << 8) | nr
}

fn bit(buf: &[u8; KEY_BYTES], code: u16) -> bool {
    let i = code as usize;
    i < KEY_BYTES * 8 && buf[i / 8] & (1 << (i % 8)) != 0
}

fn bits(f: &std::fs::File, nr: u64) -> Option<[u8; KEY_BYTES]> {
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; KEY_BYTES];
    // SAFETY: EVIOCGBIT/EVIOCGKEY write at most KEY_BYTES (encoded in the request) into buf.
    let r = unsafe { ioctl(f.as_raw_fd(), eviocg(nr), buf.as_mut_ptr()) };
    (r >= 0).then_some(buf)
}

/// every input device that has this key (read-only, events are never consumed)
fn keyboards(code: u16) -> Vec<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(dir) = std::fs::read_dir("/dev/input") else { return vec![] };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .filter_map(|e| std::fs::OpenOptions::new().read(true).custom_flags(0o4000).open(e.path()).ok())
        .filter(|f| bits(f, 0x20 + 1).is_some_and(|b| bit(&b, code))) // EVIOCGBIT(EV_KEY)
        .collect()
}

fn key_down(f: &std::fs::File, code: u16) -> bool {
    bits(f, 0x18).is_some_and(|b| bit(&b, code)) // EVIOCGKEY
}

/// Linux key code for a key name ("X", "F9", "KEY_X", or a number).
pub fn key_code(name: &str) -> Option<u16> {
    let n = name.trim().to_uppercase();
    let n = n.strip_prefix("KEY_").unwrap_or(&n);
    if let Ok(c) = n.parse::<u16>() {
        return (n.len() > 1).then_some(c).or(match c {
            0 => Some(11),
            c => Some(c + 1), // single digit key
        });
    }
    if let Some(f) = n.strip_prefix('F').and_then(|d| d.parse::<u16>().ok()) {
        return match f {
            1..=10 => Some(58 + f),
            11 | 12 => Some(76 + f),
            _ => None,
        };
    }
    let rows = ["QWERTYUIOP", "ASDFGHJKL", "ZXCVBNM"];
    let starts = [16u16, 30, 44];
    if n.len() == 1 {
        for (row, start) in rows.iter().zip(starts) {
            if let Some(i) = row.find(n) {
                return Some(start + i as u16);
            }
        }
    }
    match n {
        "SPACE" => Some(57),
        "GRAVE" => Some(41),
        _ => None,
    }
}

/// RMS of 16-bit little-endian PCM after the 44-byte WAV header (0 if too short).
pub fn rms(wav: &[u8]) -> f64 {
    let pcm = wav.get(44..).unwrap_or(&[]);
    let n = pcm.len() / 2;
    if n < 1600 {
        return 0.0;
    }
    let sum: f64 = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            let s = i16::from_le_bytes(*c) as f64;
            s * s
        })
        .sum();
    (sum / n as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names() {
        assert_eq!(key_code("X"), Some(45));
        assert_eq!(key_code("key_q"), Some(16));
        assert_eq!(key_code("M"), Some(50));
        assert_eq!(key_code("F9"), Some(67));
        assert_eq!(key_code("F12"), Some(88));
        assert_eq!(key_code("1"), Some(2));
        assert_eq!(key_code("0"), Some(11));
        assert_eq!(key_code("45"), Some(45));
        assert_eq!(key_code("space"), Some(57));
        assert_eq!(key_code("nope"), None);
    }

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
