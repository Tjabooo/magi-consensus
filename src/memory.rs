//! Short conversation memory: the last few exchanges (what you said → what MAGI answered or did),
//! so follow-ups ("and in Swedish?", "do that again") make sense. Lives in
//! `$XDG_STATE_HOME/magi-consensus/conversation.jsonl`; turns older than `planner.memory_minutes`
//! are ignored, and the file keeps only the last 50 lines.

use crate::config::{self, Config};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP: usize = 50;

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn path() -> std::path::PathBuf {
    config::state_dir().join("conversation.jsonl")
}

/// Recent turns as "User: … / You: …" lines, oldest first ("" when there are none).
pub fn recent(cfg: &Config) -> String {
    if cfg.planner.memory_turns == 0 {
        return String::new();
    }
    let since = now_s().saturating_sub(cfg.planner.memory_minutes * 60);
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let turns: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["t"].as_u64().unwrap_or(0) >= since)
        .collect();
    let mut s = String::new();
    for v in &turns[turns.len().saturating_sub(cfg.planner.memory_turns)..] {
        s.push_str(&format!("User: {}\nYou: {}\n", v["user"].as_str().unwrap_or(""), v["you"].as_str().unwrap_or("")));
    }
    s
}

/// Remember one exchange. `you` = what was said back or done.
pub fn add(cfg: &Config, user: &str, you: &str) {
    if cfg.planner.memory_turns == 0 || user.trim().is_empty() {
        return;
    }
    let p = path();
    let _ = std::fs::create_dir_all(config::state_dir());
    let mut lines: Vec<String> = std::fs::read_to_string(&p).unwrap_or_default().lines().map(str::to_string).collect();
    lines.push(json!({"t": now_s(), "user": user.trim(), "you": you.trim()}).to_string());
    let start = lines.len().saturating_sub(KEEP);
    let _ = std::fs::write(&p, lines[start..].join("\n") + "\n");
}

/// Forget the conversation ("forget that", `consensus forget`).
pub fn clear() {
    let _ = std::fs::remove_file(path());
}
