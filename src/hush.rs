//! Barge-in: saying the wake word (or pressing the talk key) while MAGI is still busy with the last
//! request cuts her off. Works across processes through two state files:
//! - `tts.pid`: the process group of the voice that is speaking now; an interrupt kills it at once;
//! - `interrupt.at`: when the last interrupt happened; a request that started before it gives up at
//!   its next checkpoint (it doesn't run its command or say anything).

use crate::config;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn at_file() -> PathBuf {
    config::state_dir().join("interrupt.at")
}

fn tts_file() -> PathBuf {
    config::state_dir().join("tts.pid")
}

/// Cut off whatever MAGI is saying or about to do. True if a voice was speaking.
pub fn interrupt() -> bool {
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(at_file(), now().to_string());
    let Ok(pid) = std::fs::read_to_string(tts_file()) else { return false };
    let _ = std::fs::remove_file(tts_file());
    let pid = pid.trim();
    if pid.is_empty() || !PathBuf::from(format!("/proc/{pid}")).exists() {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Was there an interrupt after `since` (a `now()` value)?
pub fn interrupted_since(since: u128) -> bool {
    std::fs::read_to_string(at_file()).ok().and_then(|s| s.trim().parse::<u128>().ok()).is_some_and(|t| t > since)
}

/// A voice started speaking in process group `pgid`.
pub fn speaking(pgid: u32) {
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(tts_file(), pgid.to_string());
}

/// That voice is done (only clears the file if it is still ours).
pub fn done_speaking(pgid: u32) {
    if std::fs::read_to_string(tts_file()).is_ok_and(|p| p.trim() == pgid.to_string()) {
        let _ = std::fs::remove_file(tts_file());
    }
}

/// "MAGI, stop" / "never mind" / "tyst": a request that only means "be quiet". Handled by saying
/// nothing at all (the interrupt already did the work).
pub fn is_hush(request: &str) -> bool {
    let r: String =
        request.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == ' ' { c } else { ' ' }).collect();
    let r = r
        .split_whitespace()
        .filter(|w| !matches!(*w, "please" | "magi" | "maggie" | "snälla"))
        .collect::<Vec<_>>()
        .join(" ");
    matches!(
        r.as_str(),
        "" | "stop"
            | "stop it"
            | "stop talking"
            | "shut up"
            | "be quiet"
            | "quiet"
            | "silence"
            | "hush"
            | "never mind"
            | "nevermind"
            | "cancel"
            | "cancel that"
            | "forget it"
            | "no"
            | "no no"
            | "wait"
            | "tyst"
            | "var tyst"
            | "håll käften"
            | "sluta"
            | "glöm det"
            | "strunta i det"
            | "avbryt"
            | "nej"
            | "vänta"
            | "停"
            | "停下"
            | "别说了"
            | "算了"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hush_words() {
        for r in ["stop", "Stop!", "never mind.", "Shut up please", "tyst", "Glöm det.", "算了", ""] {
            assert!(is_hush(r), "{r:?}");
        }
        for r in ["stop writing", "stop the music", "open discord", "nej tack till kaffe"] {
            assert!(!is_hush(r), "{r:?}");
        }
    }
}
