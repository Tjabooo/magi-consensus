//! What the person sees and hears: the vote card, the spoken reply, and the log.

use crate::config::{self, Config, Notify};
use crate::council::Ballot;
use serde_json::Value;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

const TAG: &str = "magi-consensus";

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn short(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        one.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    } else {
        one
    }
}

pub struct Card {
    pub title: String,
    pub lines: Vec<String>,
    pub critical: bool,
}

impl Card {
    pub fn new(title: impl Into<String>) -> Self {
        Card { title: title.into(), lines: Vec::new(), critical: false }
    }
}

/// Body helpers that respect `notify.markup`.
pub struct Fmt<'a>(pub &'a Notify);

impl Fmt<'_> {
    pub fn color(&self, key: &str, text: &str) -> String {
        if self.0.markup {
            format!("<span foreground='{}'>{}</span>", self.0.color(key), esc(text))
        } else {
            text.to_string()
        }
    }
    pub fn bold(&self, key: &str, text: &str) -> String {
        if self.0.markup {
            format!("<b><span foreground='{}'>{}</span></b>", self.0.color(key), esc(text))
        } else {
            text.to_string()
        }
    }
    pub fn plain(&self, text: &str) -> String {
        if self.0.markup {
            esc(text)
        } else {
            text.to_string()
        }
    }
    pub fn command(&self, cmd: &str) -> String {
        self.color("accent", &format!("$ {}", short(cmd, 140)))
    }
    pub fn ballot(&self, b: &Ballot) -> String {
        let (key, word) = if b.approve { ("approve", "APPROVE") } else { ("deny", "DENY   ") };
        let name = format!("{:<9}", b.unit);
        if self.0.markup {
            format!(
                "<tt>{} {}</tt> {}",
                self.bold(key, &name),
                self.bold(key, word),
                self.color("dim", &short(&b.reason, 90))
            )
        } else {
            format!("{name} {word} {}", short(&b.reason, 90))
        }
    }
}

/// Show (or replace) the single MAGI card.
pub fn card(cfg: &Config, c: &Card) {
    card_for(cfg, c, cfg.notify.timeout_ms);
}

/// Take the card down (replaced by one that expires at once).
pub fn close_card(cfg: &Config) {
    card_for(cfg, &Card::new("MAGI"), 1);
}

/// The card with its own lifetime in ms.
pub fn card_for(cfg: &Config, c: &Card, timeout_ms: i64) {
    let n = &cfg.notify;
    if !n.enabled {
        return;
    }
    let _ = Command::new(&n.command)
        .args([
            "-a",
            &n.app_name,
            "-h",
            &format!("string:x-dunst-stack-tag:{TAG}"),
            "-h",
            &format!("string:x-canonical-private-synchronous:{TAG}"),
            "-u",
            if c.critical { "critical" } else { "normal" },
            "-t",
            &timeout_ms.to_string(),
            &c.title,
            &c.lines.join("\n"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Speak `text` through `voice.tts` (blocks until it has been said).
pub fn speak(cfg: &Config, text: &str) {
    speak_in(cfg, text, "");
}

/// Speak in a language's own voice (`voice.tts_voices`), else the default `voice.tts`.
pub fn speak_in(cfg: &Config, text: &str, lang: &str) {
    let text = text.trim();
    let tts = cfg.voice.tts_voices.get(lang).filter(|t| !t.trim().is_empty()).unwrap_or(&cfg.voice.tts);
    if tts.trim().is_empty() || text.is_empty() {
        return;
    }
    // own process group, recorded so saying the wake word can kill the whole voice pipeline at once
    let child = Command::new("sh")
        .args(["-c", tts])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    match child {
        Ok(mut ch) => {
            let pgid = ch.id();
            crate::hush::speaking(pgid);
            if let Some(mut i) = ch.stdin.take() {
                let _ = writeln!(i, "{text}");
            }
            let res = ch.wait_with_output();
            crate::hush::done_speaking(pgid);
            match res {
                Ok(o) if o.status.code().is_none() => {} // cut off (interrupted)
                Ok(o) if !o.status.success() => {
                    eprintln!("tts failed: {}", String::from_utf8_lossy(&o.stderr).trim());
                }
                Err(e) => eprintln!("tts failed: {e}"),
                _ => {}
            }
        }
        Err(e) => eprintln!("tts: can't start sh: {e}"),
    }
}

/// Append to votes.jsonl and replace last.json.
pub fn log(record: &Value) {
    let dir = config::state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("votes.jsonl")) {
        let _ = writeln!(f, "{record}");
    }
    let tmp = dir.join("last.json.tmp");
    if std::fs::write(&tmp, record.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("last.json"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_collapses_and_cuts() {
        assert_eq!(short("a\n  b", 10), "a b");
        assert_eq!(short("abcdefghij", 5), "abcd…");
    }
}
