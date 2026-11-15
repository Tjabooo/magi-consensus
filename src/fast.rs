//! Fast path: phrases you wrote yourself in `[[commands]]` run at once, with no model and no vote.
//! A request can chain several ("workspace 3 and fullscreen roblox"); it only takes the fast path
//! when every part matches a command, otherwise the whole request goes to the council.

use crate::config::{App, Config, FastCommand};
use crate::exec;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One matched part of the request.
#[derive(Debug, Clone)]
pub struct Step {
    pub phrase: String,
    pub command: usize,
    pub slots: HashMap<String, String>,
    pub target: Option<Target>,
}

/// What `{app}` points at. Resolved again when the step runs (a launched app appears later).
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// a configured app (running or not)
    App(usize),
    /// an open window whose class matched the words
    Class(String),
    /// "it", "this", "that": the app of the previous step, else the focused window
    Previous,
}

#[derive(Debug, Clone, Default)]
pub struct Window {
    pub address: String,
    pub class: String,
    pub title: String,
    pub pid: i64,
    pub workspace: String,
    pub fullscreen: i64,
    pub focus: i64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub ok: bool,
    pub lines: Vec<String>,
    pub speech: Vec<String>,
}

// ── text ─────────────────────────────────────────────────────────────────────

const UNITS: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

fn unit(w: &str) -> Option<u32> {
    UNITS.iter().position(|u| *u == w).map(|i| i as u32)
}

fn tens(w: &str) -> Option<u32> {
    TENS.iter().position(|t| !t.is_empty() && *t == w).map(|i| i as u32 * 10)
}

/// "twenty five" → "25", "one hundred" → "100"; other words untouched.
fn numbers(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        let mut val = None;
        let mut used = 1;
        if let Some(t) = tens(w) {
            if let Some(u) = words.get(i + 1).and_then(|n| unit(n)).filter(|u| (1..10).contains(u)) {
                val = Some(t + u);
                used = 2;
            } else {
                val = Some(t);
            }
        } else if let Some(u) = unit(w) {
            val = Some(u);
        }
        if let Some(v) = val {
            if words.get(i + used).map(String::as_str) == Some("hundred") {
                val = Some(v * 100);
                used += 1;
            }
        } else if w == "hundred" {
            val = Some(100);
        }
        match val {
            Some(v) => out.push(v.to_string()),
            None => out.push(w.to_string()),
        }
        i += used;
    }
    // Whisper writes "workspace to" for "workspace two" now and then
    for j in 1..out.len() {
        if out[j - 1] == "workspace" && matches!(out[j].as_str(), "to" | "too") {
            out[j] = "2".into();
        } else if out[j - 1] == "workspace" && out[j] == "for" {
            out[j] = "4".into();
        }
    }
    out
}

/// Lower case, punctuation → " , " separators, % → percent, number words → digits.
pub fn normalize(text: &str) -> String {
    let mut s = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '%' => s.push_str(" percent "),
            '\'' | '’' => {}
            ',' | '.' | ';' | '!' | '?' | ':' => s.push_str(" , "),
            c if c.is_alphanumeric() => s.extend(c.to_lowercase()),
            _ => s.push(' '),
        }
    }
    let words: Vec<String> = s.split_whitespace().map(str::to_string).collect();
    numbers(words).join(" ")
}

fn strip_words(mut s: &str, list: &[String], lead: bool) -> String {
    loop {
        let before = s;
        for f in list {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            if lead {
                if s == f {
                    s = "";
                } else if let Some(r) = s.strip_prefix(f).and_then(|r| r.strip_prefix(' ')) {
                    s = r;
                }
            } else if s == f {
                s = "";
            } else if let Some(r) = s.strip_suffix(f).and_then(|r| r.strip_suffix(' ')) {
                s = r;
            }
        }
        s = s.trim_matches(|c: char| c == ',' || c == ' ');
        if s == before {
            return s.to_string();
        }
    }
}

/// Parts of a request: split on punctuation and "and"/"then"; wake words and fillers removed.
pub fn parts(cfg: &Config, normalized: &str) -> Vec<String> {
    let mut lead: Vec<String> = cfg.fast.wake_words.clone();
    lead.extend(cfg.fast.lead_fillers.iter().cloned());
    let re = Regex::new(r"\s*(?:,|\band then\b|\bthen\b|\band\b|\balso\b|\bafter that\b)\s*").unwrap();
    re.split(normalized)
        .map(|p| strip_words(&strip_words(p.trim(), &lead, true), &cfg.fast.tail_fillers, false))
        .filter(|p| !p.is_empty())
        .collect()
}

/// The request without wake words, fillers and separators, as one phrase.
fn whole(cfg: &Config, normalized: &str) -> String {
    let mut lead: Vec<String> = cfg.fast.wake_words.clone();
    lead.extend(cfg.fast.lead_fillers.iter().cloned());
    let flat = normalized.replace(" , ", " ").replace(',', " ");
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    strip_words(&strip_words(&flat, &lead, true), &cfg.fast.tail_fillers, false)
}

// ── patterns ─────────────────────────────────────────────────────────────────

/// `switch to workspace {n}` / `[the|my] mic (off|mute)` → anchored regex over "phrase ".
pub fn compile(pattern: &str) -> Result<Regex, String> {
    let mut re = String::from("^");
    for tok in normalize_pattern(pattern).split_whitespace() {
        let piece = if let Some(name) = tok.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            match name {
                "n" | "level" => format!("(?P<{name}>\\d+) "),
                "app" | "text" => format!("(?P<{name}>.+?) "),
                _ => return Err(format!("unknown slot {{{name}}} in \"{pattern}\"")),
            }
        } else if let Some(inner) = tok.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            format!("(?:(?:{}) )?", alts(inner))
        } else if let Some(inner) = tok.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
            format!("(?:{}) ", alts(inner))
        } else {
            format!("{} ", regex::escape(tok))
        };
        re.push_str(&piece);
    }
    re.push('$');
    Regex::new(&re).map_err(|e| format!("\"{pattern}\": {e}"))
}

/// Patterns go through the same normalisation as speech (number words, case), keeping the syntax.
fn normalize_pattern(p: &str) -> String {
    p.split_whitespace()
        .map(|t| if t.starts_with('{') || t.contains(['[', '(', '|']) { t.to_lowercase() } else { normalize(t) })
        .collect::<Vec<_>>()
        .join(" ")
}

fn alts(inner: &str) -> String {
    // "a b" inside brackets is written a_b (one token); underscores become spaces
    inner
        .split('|')
        .map(|a| a.split('_').map(|w| regex::escape(&normalize(w))).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("|")
}

// ── windows ──────────────────────────────────────────────────────────────────

fn hyprctl_json(what: &str) -> Value {
    Command::new("hyprctl")
        .args([what, "-j"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or(Value::Null)
}

fn window_of(v: &Value) -> Window {
    Window {
        address: v["address"].as_str().unwrap_or("").to_string(),
        class: v["class"].as_str().unwrap_or("").to_string(),
        title: v["title"].as_str().unwrap_or("").to_string(),
        pid: v["pid"].as_i64().unwrap_or(-1),
        workspace: v["workspace"]["name"].as_str().unwrap_or("").to_string(),
        fullscreen: v["fullscreen"].as_i64().unwrap_or(0),
        focus: v["focusHistoryID"].as_i64().unwrap_or(i64::MAX),
    }
}

pub fn windows() -> Vec<Window> {
    let mut w: Vec<Window> = match hyprctl_json("clients") {
        Value::Array(a) => a.iter().map(window_of).filter(|w| !w.address.is_empty()).collect(),
        _ => vec![],
    };
    w.sort_by_key(|w| w.focus);
    w
}

fn active() -> Option<Window> {
    let w = window_of(&hyprctl_json("activewindow"));
    (!w.address.is_empty()).then_some(w)
}

/// Lower-cased command lines of every process below `pid`.
fn descendants(pid: i64) -> Vec<String> {
    let mut parent: HashMap<i64, Vec<i64>> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Ok(p) = e.file_name().to_string_lossy().parse::<i64>() else { continue };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else { continue };
            let ppid = stat.rsplit(')').next().and_then(|r| r.split_whitespace().nth(1)).and_then(|x| x.parse().ok());
            if let Some(pp) = ppid {
                parent.entry(pp).or_default().push(p);
            }
        }
    }
    let mut out = Vec::new();
    let mut todo = vec![pid];
    while let Some(p) = todo.pop() {
        if let Ok(c) = std::fs::read(format!("/proc/{p}/cmdline")) {
            out.push(String::from_utf8_lossy(&c).replace('\0', " ").to_lowercase());
        }
        if let Some(kids) = parent.get(&p) {
            todo.extend(kids);
        }
        if out.len() > 500 {
            break;
        }
    }
    out
}

fn regex_hit(pattern: &str, text: &str) -> bool {
    !pattern.is_empty() && Regex::new(pattern).map(|r| r.is_match(text)).unwrap_or(false)
}

/// Windows of a configured app, most recently focused first.
pub fn app_windows(app: &App, all: &[Window]) -> Vec<Window> {
    all.iter()
        .filter(|w| {
            let class_ok = app.class.is_empty() || regex_hit(&app.class, &w.class);
            let title_ok = app.title.is_empty() || regex_hit(&app.title, &w.title);
            let has_rule = !app.class.is_empty() || !app.title.is_empty() || !app.process.is_empty();
            let proc_ok =
                app.process.is_empty() || descendants(w.pid).iter().any(|c| c.contains(&app.process.to_lowercase()));
            has_rule && class_ok && title_ok && proc_ok
        })
        .cloned()
        .collect()
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Words → app. Configured names and aliases first, then the class of an open window.
pub fn resolve(cfg: &Config, words: &str, all: &[Window]) -> Option<Target> {
    let w = strip_words(words, &["the".into(), "my".into(), "a".into()], true);
    if matches!(w.as_str(), "it" | "this" | "that" | "this window" | "that window" | "the window" | "this app") {
        return Some(Target::Previous);
    }
    let sw = squash(&w);
    if sw.is_empty() {
        return None;
    }
    for (i, a) in cfg.apps.iter().enumerate() {
        if std::iter::once(&a.name).chain(a.aliases.iter()).any(|n| squash(n) == sw) {
            return Some(Target::App(i));
        }
    }
    if sw.len() >= 3 {
        // class parts: org.vinegarhq.Sober → org, vinegarhq, sober
        let hit = all.iter().find(|win| {
            let c = win.class.to_lowercase();
            c.split(['.', '-', '_', ' ']).any(|p| squash(p).starts_with(&sw)) || squash(&c) == sw
        });
        if let Some(win) = hit {
            return Some(Target::Class(win.class.clone()));
        }
    }
    None
}

// ── matching ─────────────────────────────────────────────────────────────────

fn match_one(cfg: &Config, compiled: &[(usize, Regex)], phrase: &str, all: &[Window]) -> Option<Step> {
    let hay = format!("{phrase} ");
    for (ci, re) in compiled {
        let Some(caps) = re.captures(&hay) else { continue };
        let mut slots = HashMap::new();
        let mut target = None;
        let mut ok = true;
        for name in re.capture_names().flatten() {
            let Some(m) = caps.name(name) else { continue };
            let v = m.as_str().trim().to_string();
            if name == "app" {
                match resolve(cfg, &v, all) {
                    Some(t) => target = Some(t),
                    None => ok = false,
                }
            }
            slots.insert(name.to_string(), v);
        }
        if ok {
            return Some(Step { phrase: phrase.to_string(), command: *ci, slots, target });
        }
    }
    None
}

fn compiled(cfg: &Config) -> Vec<(usize, Regex)> {
    let mut out = Vec::new();
    for (i, c) in cfg.commands.iter().enumerate() {
        for p in &c.say {
            match compile(p) {
                Ok(r) => out.push((i, r)),
                Err(e) => eprintln!("commands: {e}"),
            }
        }
    }
    out
}

/// The steps, if every part of the request is a known command.
pub fn plan(cfg: &Config, request: &str) -> Option<Vec<Step>> {
    if !cfg.fast.enabled || cfg.commands.is_empty() {
        return None;
    }
    let norm = normalize(request);
    let all = windows();
    let compiled = compiled(cfg);
    let w = whole(cfg, &norm);
    if w.is_empty() {
        return None;
    }
    if let Some(s) = match_one(cfg, &compiled, &w, &all) {
        return Some(vec![s]);
    }
    let parts = parts(cfg, &norm);
    if parts.len() < 2 {
        return None;
    }
    parts.iter().map(|p| match_one(cfg, &compiled, p, &all)).collect()
}

// ── running ──────────────────────────────────────────────────────────────────

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn dispatch(lua: &str) -> Result<(), String> {
    let o = Command::new("hyprctl").args(["dispatch", lua]).output().map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.success() && (out.is_empty() || out == "ok") {
        Ok(())
    } else {
        Err(format!("hyprctl: {out}"))
    }
}

fn lua_str(s: &str) -> String {
    format!("[==[{s}]==]")
}

struct Ctx<'a> {
    cfg: &'a Config,
    previous: Option<Target>,
    /// launched by an earlier step: later steps wait for its window
    launched: Option<Target>,
}

/// Find the window for a target now; waits up to `wait` for a just-launched app.
fn find(cfg: &Config, t: &Target, wait: Duration) -> Option<Window> {
    let t0 = Instant::now();
    loop {
        let all = windows();
        let hit = match t {
            Target::App(i) => app_windows(&cfg.apps[*i], &all).into_iter().next(),
            Target::Class(c) => all.iter().find(|w| &w.class == c).cloned(),
            Target::Previous => None,
        };
        if hit.is_some() || t0.elapsed() >= wait {
            return hit;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn target_name(cfg: &Config, t: &Target) -> String {
    match t {
        Target::App(i) => cfg.apps[*i].name.clone(),
        Target::Class(c) => c.clone(),
        Target::Previous => "it".into(),
    }
}

fn run_step(ctx: &mut Ctx, step: &Step) -> Result<Option<String>, String> {
    let cfg = ctx.cfg;
    let cmd: &FastCommand = &cfg.commands[step.command];
    let target = match &step.target {
        Some(Target::Previous) => ctx.previous.clone().or(Some(Target::Previous)),
        t => t.clone(),
    };
    let launch_wait = Duration::from_secs(cfg.fast.launch_wait_s);
    // the window: the target's, else the focused one
    let window = match &target {
        None | Some(Target::Previous) => active(),
        Some(t) => find(cfg, t, if ctx.launched.as_ref() == Some(t) { launch_wait } else { Duration::ZERO }),
    };
    let app = match &target {
        Some(Target::App(i)) => Some(&cfg.apps[*i]),
        _ => None,
    };
    let n = step.slots.get("n").cloned().unwrap_or_default();
    let need_window = || {
        window.clone().ok_or_else(|| match &target {
            Some(t) => format!("{} isn't open", target_name(cfg, t)),
            None => "no window is focused".to_string(),
        })
    };
    if let Some(t) = &target {
        if *t != Target::Previous {
            ctx.previous = Some(t.clone());
        }
    }
    let run = cmd.run.trim();
    let (verb, arg) = run.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((run, ""));
    match verb {
        "@workspace" => dispatch(&format!("hl.dsp.focus({{ workspace = {} }})", lua_str(&n))).map(|_| None),
        "@focus" => {
            let w = need_window()?;
            if let Some(sp) = w.workspace.strip_prefix("special:") {
                // a scratchpad: show it instead of focusing a hidden window
                return dispatch(&format!("hl.dsp.workspace.toggle_special({})", lua_str(sp))).map(|_| None);
            }
            dispatch(&format!("hl.dsp.focus({{ window = \"address:{}\" }})", w.address)).map(|_| None)
        }
        "@open" => {
            if let Some(w) = &window {
                if let Some(sp) = w.workspace.strip_prefix("special:") {
                    return dispatch(&format!("hl.dsp.workspace.toggle_special({})", lua_str(sp))).map(|_| None);
                }
                return dispatch(&format!("hl.dsp.focus({{ window = \"address:{}\" }})", w.address)).map(|_| None);
            }
            let launch = app.map(|a| a.launch.trim()).filter(|l| !l.is_empty()).ok_or_else(|| {
                format!(
                    "{} isn't open and has no launch command",
                    target.as_ref().map(|t| target_name(cfg, t)).unwrap_or_default()
                )
            })?;
            dispatch(&format!("hl.dsp.exec_cmd({})", lua_str(&crate::config::expand_cmd(launch))))?;
            ctx.launched = target.clone();
            Ok(None)
        }
        "@close" => {
            let w = need_window()?;
            dispatch(&format!("hl.dsp.focus({{ window = \"address:{}\" }})", w.address))?;
            std::thread::sleep(Duration::from_millis(60));
            dispatch("hl.dsp.window.close()").map(|_| None)
        }
        "@fullscreen" | "@unfullscreen" => {
            let w = need_window()?;
            let want = verb == "@fullscreen";
            if (w.fullscreen == 2) == want {
                return Ok(None);
            }
            dispatch(&format!("hl.dsp.focus({{ window = \"address:{}\" }})", w.address))?;
            if want && !cfg.fast.fullscreen_cmd.trim().is_empty() {
                let c = cfg.fast.fullscreen_cmd.replace("{address}", &w.address);
                let o = exec::run(&cfg.run, &crate::config::expand_cmd(&c), Duration::from_secs(cfg.run.timeout));
                return if o.ok() { Ok(None) } else { Err(format!("fullscreen failed: {}", o.stderr.trim())) };
            }
            let action = if want { "set" } else { "unset" };
            dispatch(&format!(
                "hl.dsp.window.fullscreen({{ action = \"{action}\", window = \"address:{}\" }})",
                w.address
            ))
            .map(|_| None)
        }
        "@move" => {
            let w = need_window()?;
            dispatch(&format!(
                "hl.dsp.window.move({{ workspace = {}, window = \"address:{}\" }})",
                lua_str(&n),
                w.address
            ))
            .map(|_| None)
        }
        "@special" => dispatch(&format!("hl.dsp.workspace.toggle_special({})", lua_str(arg))).map(|_| None),
        v if v.starts_with('@') => Err(format!("unknown action {v}")),
        _ => {
            let mut c = cmd.run.clone();
            for (k, v) in &step.slots {
                let val = if k == "text" || k == "app" { quote(v) } else { v.clone() };
                c = c.replace(&format!("{{{k}}}"), &val);
            }
            let w = window.clone().unwrap_or_default();
            c = c
                .replace("{app.address}", &w.address)
                .replace("{app.class}", &quote(&w.class))
                .replace("{app.title}", &quote(&w.title))
                .replace("{app.workspace}", &quote(&w.workspace))
                .replace("{app.fullscreen}", &w.fullscreen.to_string())
                .replace("{app.pid}", &w.pid.to_string());
            let o = exec::run(&cfg.run, &c, Duration::from_secs(cfg.run.timeout));
            if !o.ok() {
                let why = if o.timed_out { "timed out".to_string() } else { o.stderr.trim().to_string() };
                return Err(if why.is_empty() { format!("exit {}", o.code.unwrap_or(-1)) } else { why });
            }
            Ok(cmd.speak_output.then(|| o.stdout.trim().to_string()).filter(|s| !s.is_empty()))
        }
    }
}

pub fn run(cfg: &Config, steps: &[Step]) -> Report {
    let mut ctx = Ctx { cfg, previous: None, launched: None };
    let mut rep = Report { ok: true, ..Default::default() };
    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(cfg.fast.step_delay_ms));
        }
        let cmd = &cfg.commands[s.command];
        match run_step(&mut ctx, s) {
            Ok(out) => {
                rep.lines.push(format!("✓ {}", s.phrase));
                if !cmd.reply.trim().is_empty() {
                    rep.speech.push(cmd.reply.trim().to_string());
                }
                if let Some(o) = out {
                    let cap = cfg.voice.speak_output_chars;
                    rep.speech.push(o.chars().take(cap).collect());
                }
            }
            Err(e) => {
                rep.ok = false;
                rep.lines.push(format!("✗ {}: {e}", s.phrase));
                rep.speech.push(format!("Couldn't {}. {e}.", s.phrase));
                break;
            }
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn cfg() -> Config {
        config::parse(Some(
            r#"
[[apps]]
name = "roblox"
aliases = ["sober", "steal an egg"]
class = "^org\\.vinegarhq\\.Sober$"
launch = "flatpak run org.vinegarhq.Sober"
"#,
        ))
        .unwrap()
    }

    #[test]
    fn numbers_and_punctuation() {
        assert_eq!(normalize("MAGI, switch to workspace three!"), "magi , switch to workspace 3 ,");
        assert_eq!(normalize("Volume twenty five %"), "volume 25 percent");
        assert_eq!(normalize("go to workspace to"), "go to workspace 2");
        assert_eq!(normalize("one hundred"), "100");
    }

    #[test]
    fn split_and_strip() {
        let c = cfg();
        let n = normalize("Magi, switch to workspace 3 and fullscreen Roblox, please.");
        assert_eq!(parts(&c, &n), vec!["switch to workspace 3", "fullscreen roblox"]);
        assert_eq!(whole(&c, &normalize("Hey Maggie, clip that.")), "clip that");
    }

    #[test]
    fn patterns() {
        let r = compile("[switch|go] to workspace {n}").unwrap();
        assert!(r.is_match("switch to workspace 3 "));
        assert!(r.is_match("to workspace 3 "));
        assert!(!r.is_match("switch to workspace three "));
        let r = compile("(skip|next) [song|track]").unwrap();
        assert!(r.is_match("skip "));
        assert!(r.is_match("next song "));
        assert!(!r.is_match("song "));
        let r = compile("[make] {app} fullscreen").unwrap();
        assert_eq!(&r.captures("make steal an egg fullscreen ").unwrap()["app"], "steal an egg");
        let r = compile("[what_is|whats] my ping").unwrap();
        assert!(r.is_match("what is my ping "));
        assert!(r.is_match("whats my ping "));
        assert!(compile("{nope}").is_err());
    }

    #[test]
    fn resolve_names() {
        let c = cfg();
        assert_eq!(resolve(&c, "the sober", &[]), Some(Target::App(0)));
        assert_eq!(resolve(&c, "Steal an egg", &[]), Some(Target::App(0)));
        assert_eq!(resolve(&c, "it", &[]), Some(Target::Previous));
        let w = vec![Window { class: "com.obsproject.Studio".into(), address: "0x1".into(), ..Default::default() }];
        assert_eq!(resolve(&c, "obs", &w), Some(Target::Class("com.obsproject.Studio".into())));
        assert_eq!(resolve(&c, "spreadsheet", &w), None);
    }

    #[test]
    fn plan_needs_every_part() {
        let c = cfg();
        let all = vec![];
        let comp = compiled(&c);
        assert!(match_one(&c, &comp, "switch to workspace 3", &all).is_some());
        assert!(match_one(&c, &comp, "fullscreen roblox", &all).is_some());
        assert!(match_one(&c, &comp, "write me a poem", &all).is_none());
    }
}
