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

// ── Standing notes: things to remember for good ("answer my Chinese in English", "my name is …") ──

fn notes_path() -> std::path::PathBuf {
    config::state_dir().join("notes.json")
}

const MAX_NOTES: usize = 30;

pub fn notes() -> Vec<String> {
    std::fs::read_to_string(notes_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .unwrap_or_default()
}

fn save_notes(n: &[String]) {
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(notes_path(), serde_json::to_string_pretty(n).unwrap_or_else(|_| "[]".into()));
}

/// Apply the planner's note changes: `forget` = a note's text (or "all"), `remember` = a new note.
/// Returns what changed, for the card.
pub fn update_notes(cfg: &Config, request: &str, remember: &str, forget: &str) -> Vec<String> {
    let (mut remember, forget) = (remember.trim(), forget.trim());
    // only a request that says something lasting makes a note (a one-off "answer in English" doesn't)
    if !remember.is_empty() && !has_note_cue(cfg, request) {
        eprintln!("notes: ignoring {remember:?}: the request has no lasting words (planner.note_cues)");
        remember = "";
    }
    // "stop doing X": the planner sometimes forgets X and notes it again in the same breath
    if !forget.is_empty() && !remember.is_empty() {
        let (r, f) = (remember.to_lowercase(), forget.to_lowercase());
        if forget.eq_ignore_ascii_case("all") || r.contains(&f) || f.contains(&r) {
            remember = "";
        }
    }
    if remember.is_empty() && forget.is_empty() {
        return vec![];
    }
    let mut n = notes();
    let mut changed = Vec::new();
    if forget.eq_ignore_ascii_case("all") {
        if !n.is_empty() {
            changed.push(format!("forgot {} notes", n.len()));
        }
        n.clear();
    } else if !forget.is_empty() {
        let f = forget.to_lowercase();
        let before = n.len();
        n.retain(|x| {
            let x = x.to_lowercase();
            !(x == f || x.contains(&f) || f.contains(&x))
        });
        if n.len() < before {
            changed.push(format!("forgot: {forget}"));
        }
    }
    // a one- or two-word note ("CS2") loses what it was about: keep the request's own words instead
    let full;
    if !remember.is_empty() && remember.split_whitespace().count() < 3 {
        full = request.trim().to_string();
        remember = &full;
    }
    if !remember.is_empty() && !n.iter().any(|x| x.eq_ignore_ascii_case(remember)) {
        n.push(remember.to_string());
        changed.push(format!("noted: {remember}"));
        let over = n.len().saturating_sub(MAX_NOTES);
        n.drain(..over);
    }
    save_notes(&n);
    changed
}

/// Whether the request has a word that makes something lasting ("from now on", "always", "remember"…).
pub fn has_note_cue(cfg: &Config, request: &str) -> bool {
    let r = request.to_lowercase();
    cfg.planner.note_cues.iter().any(|c| !c.trim().is_empty() && r.contains(&c.to_lowercase()))
}

pub fn clear_notes() {
    let _ = std::fs::remove_file(notes_path());
    let _ = std::fs::remove_file(rules_path());
}

// ── Reply-language rules: "answer my Chinese in English" = zh → en ("*" = any language) ──────────

fn rules_path() -> std::path::PathBuf {
    config::state_dir().join("reply-languages.json")
}

pub fn language_rules() -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(rules_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// The language to answer `heard` in, if a rule says so.
pub fn reply_rule(heard: &str) -> Option<String> {
    let r = language_rules();
    r.get(heard).or_else(|| r.get("*")).cloned()
}

/// Set or clear a rule: reply "" or reply == heard clears it ("stop answering my Chinese in English").
/// Setting one needs a lasting word in the request (note_cues); clearing never does.
/// `spoken` = the request's own language: "answer me in English from now on" said in Chinese, with
/// no heard language given, means Chinese → English.
pub fn update_language_rule(cfg: &Config, request: &str, spoken: &str, heard: &str, reply: &str) -> Option<String> {
    let (mut heard, reply) = (heard.trim().to_lowercase(), reply.trim().to_lowercase());
    if heard.is_empty() && !reply.is_empty() {
        heard = spoken.trim().to_lowercase();
    }
    if heard.is_empty() {
        return None;
    }
    let mut r = language_rules();
    let msg = if reply.is_empty() || reply == heard {
        r.remove(&heard)?;
        format!("{heard} answered in {heard} again")
    } else {
        if !has_note_cue(cfg, request) {
            eprintln!("notes: ignoring language rule {heard} → {reply}: the request has no lasting words");
            return None;
        }
        if r.get(&heard) == Some(&reply) {
            return None;
        }
        r.insert(heard.clone(), reply.clone());
        format!("from now on: {heard} answered in {reply}")
    };
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(rules_path(), serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".into()));
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_cues() {
        let c = config::parse(None).unwrap();
        assert!(has_note_cue(&c, "from now on answer my Chinese in English"));
        assert!(has_note_cue(&c, "以后用英语回答"));
        assert!(has_note_cue(&c, "kom ihåg att jag heter Oliver"));
        assert!(!has_note_cue(&c, "你今天怎么样 answer in English"));
    }
}
