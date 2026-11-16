//! `consensus write`: compose NEW text from a brief ("an essay about Overwatch") with the model and type
//! it into the window that was focused when it was asked for. Dictated words ("type hello there") never
//! come here: the planner types those verbatim with wtype.
//!
//! `write [--send] -- BRIEF` returns at once and leaves a detached worker (own process group, so the
//! command timeout doesn't kill it) that streams the model's reply and types it line by line. The worker
//! stops typing if the focus moves (the whole text then goes on the clipboard instead), keeps chat
//! messages under Discord's limit, and never presses a bare Enter between lines (Shift+Enter in chats
//! and documents, a space in terminals and games). `write --stop` ends a running worker.

use crate::config::Config;
use crate::memory;
use crate::out::{self, Card, Fmt};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const SYSTEM: &str = "You write text for the user, and it is typed straight into the window they are in (a chat, \
a document, a text box) as if they had written it. They give a brief: what to write (an essay, a poem, a \
story, a message, a reply, a joke, a summary...), what about, and maybe length, tone or language.\n\
Output ONLY the text itself, ready to send or hand in: no preamble (\"Sure\", \"Here is\"), no comments or \
notes after it, no quotes around it, no title unless asked, no markdown headings. Separate paragraphs with \
a blank line.\n\
Language: the one named below (the language the user spoke), even if the brief was written down in \
another one; with none named, the brief's language. Only an explicit wish (\"in English\") changes it.\n\
Voice: a message, reply or post is written AS the user, in first person, to whoever it is for (\"write \
that I'm late\" = \"I'm running late\"). Essays, stories and poems are just good writing on the topic.\n\
Stick to the brief: say what it says, nothing it doesn't. A message or reply is short and casual, one \
to three sentences unless asked for more (\"a reply saying I can't play tonight\" = \"Can't play tonight, \
sorry!\"). A joke is one joke: a setup and a punchline. An essay is several real paragraphs with an \
argument; facts must be true, so stay general rather than invent details.\n\
Length: what the brief asks for; otherwise what the kind of text needs, and always within the limit \
given below.\n\
If the brief is itself just the words to write (\"lol\", \"hello there\", \"gg\"), output exactly \
those words and nothing else.\n\
This is the user's private machine and the words are their business: swearing, crude or sexual humour, \
roasts, dark topics and anything else they ask for are written as asked, without refusing, softening, \
warnings or moral comments.";

/// Where the text goes, from the focused window's class.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    /// chat apps: Shift+Enter line breaks, markdown kept, under Discord's 2000 characters
    Chat,
    /// terminals: one line (a bare newline would run a command)
    Terminal,
    /// game chats: one short line
    Game,
    /// documents, browsers, everything else: Shift+Enter line breaks, plain text
    Document,
}

impl Shape {
    fn of(class: &str) -> Shape {
        let c = class.to_lowercase();
        let any = |words: &[&str]| words.iter().any(|w| c.contains(w));
        if any(&["discord", "vesktop", "webcord", "armcord", "slack", "telegram", "signal", "element", "whatsapp"]) {
            Shape::Chat
        } else if any(&["alacritty", "kitty", "foot", "wezterm", "konsole", "terminal", "ghostty", "xterm"]) {
            Shape::Terminal
        } else if any(&["cs2", "gamescope", "steam_app", "sober", "roblox", "minecraft"]) || c == "csgo_linux64" {
            Shape::Game
        } else {
            Shape::Document
        }
    }
    fn max_chars(self) -> usize {
        match self {
            Shape::Chat => 1900,
            Shape::Game => 120,
            Shape::Terminal | Shape::Document => 20_000,
        }
    }
    fn max_tokens(self) -> u32 {
        match self {
            Shape::Chat => 800,
            Shape::Game => 80,
            Shape::Terminal | Shape::Document => 2500,
        }
    }
    fn one_line(self) -> bool {
        matches!(self, Shape::Terminal | Shape::Game)
    }
    fn hint(self) -> &'static str {
        match self {
            Shape::Chat => {
                "a chat message box: at most 1800 characters in total (the message limit), even for an essay \
                 (then a compact one); light markdown such as **bold** is fine"
            }
            Shape::Game => "a game's chat line: ONE line, at most 120 characters",
            Shape::Terminal => "a terminal: it is typed as a single line, so keep it to one paragraph",
            Shape::Document => "a document or text box: plain text, no markdown",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Window {
    address: String,
    class: String,
    title: String,
}

fn active_window() -> Option<Window> {
    let out = Command::new("hyprctl").args(["activewindow", "-j"]).stderr(Stdio::null()).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let w = Window { address: s("address"), class: s("class"), title: s("title") };
    (!w.address.is_empty()).then_some(w)
}

fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    base.join("magi-consensus")
}

fn pid_file() -> PathBuf {
    state_dir().join("write.pid")
}

/// Ends a running worker (its whole process group). True if one was running.
fn stop_running() -> bool {
    let Ok(pid) = std::fs::read_to_string(pid_file()) else { return false };
    let _ = std::fs::remove_file(pid_file());
    let pid = pid.trim();
    if pid.is_empty() || !PathBuf::from(format!("/proc/{pid}")).exists() {
        return false;
    }
    Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).status().is_ok_and(|s| s.success())
}

/// `consensus write ...` (args after the subcommand).
pub fn cmd(cfg: &Config, args: &[String]) -> i32 {
    let mut send = false;
    let mut worker = false;
    let mut print: Option<String> = None;
    let mut window = String::new();
    let mut to = String::new();
    let mut brief: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                brief.extend(it.by_ref().cloned());
            }
            "--send" => send = true,
            "--worker" => worker = true,
            "--print" => print = Some(it.next().cloned().unwrap_or_else(|| "discord".into())),
            "--window" => window = it.next().cloned().unwrap_or_default(),
            "--to" => to = it.next().cloned().unwrap_or_default(),
            "--stop" => {
                let stopped = stop_running();
                println!("{}", if stopped { "stopped writing" } else { "nothing is being written" });
                if stopped {
                    let mut c = Card::new("stopped writing");
                    c.lines.push(Fmt(&cfg.notify).color("dim", "the text so far stays where it was typed"));
                    out::card(cfg, &c);
                }
                return 0;
            }
            _ => brief.push(a.clone()),
        }
    }
    let brief = brief.join(" ").trim().to_string();
    if brief.is_empty() {
        eprintln!("usage: consensus write [--send] [--to NAME] -- BRIEF   (e.g. 'an essay about Overwatch'; --to opens NAME's chat in Discord first)\n       consensus write --stop\n       consensus write --print [CLASS] -- BRIEF   (test: print, type nothing; CLASS picks the shape, default discord)");
        return 64;
    }
    if let Some(class) = print {
        let w = Window { address: "print".into(), class, title: String::new() };
        return run_worker(cfg, &brief, Some(w), send);
    }
    if worker {
        return run_worker(cfg, &brief, active_window().filter(|w| w.address == window), send);
    }
    if !to.trim().is_empty() {
        if let Err(e) = open_chat(to.trim()) {
            eprintln!("{e}");
            return 1;
        }
    }
    let Some(w) = active_window() else {
        eprintln!("no focused window to type into");
        return 1;
    };
    if is_dictation(&brief) {
        // the words themselves ("hej på dig", "lol"): typed as said, the model would only embellish them
        let ok = wtype_text(&brief) && (!send || wtype_args(&["-k", "Return"]));
        println!("typed {:?} into {}{}", brief, w.class, if send { " and sent" } else { "" });
        return if ok { 0 } else { 1 };
    }
    stop_running(); // one writer at a time
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("can't find my own binary: {e}");
            return 1;
        }
    };
    let _ = std::fs::create_dir_all(state_dir());
    let log = std::fs::OpenOptions::new().create(true).append(true).open(state_dir().join("write.log"));
    let mut c = Command::new(exe);
    c.args(["write", "--worker", "--window", &w.address]);
    if send {
        c.arg("--send");
    }
    c.arg("--").arg(&brief).stdin(Stdio::null()).stdout(Stdio::null());
    c.stderr(log.map(Stdio::from).unwrap_or_else(|_| Stdio::null()));
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    match c.spawn() {
        Ok(child) => {
            let _ = std::fs::write(pid_file(), child.id().to_string());
            println!(
                "writing into {} ({:?}){}",
                w.class,
                Shape::of(&w.class),
                if send { ", then sending" } else { "" }
            );
            0
        }
        Err(e) => {
            eprintln!("can't start the writer: {e}");
            1
        }
    }
}

/// Types with wtype; text goes through stdin so nothing is read as an option.
fn wtype_text(text: &str) -> bool {
    let Ok(mut child) = Command::new("wtype").arg("-").stdin(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(text.as_bytes());
    }
    child.wait().is_ok_and(|s| s.success())
}

fn pause(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// `--to NAME`: focus the chat app (Discord & co.) and open NAME's DM or channel with the quick
/// switcher (Ctrl+K, type, Enter). Checked by the window title, which follows the open channel: if it
/// didn't change (and doesn't name them already), the switcher is closed again and nothing is typed.
fn open_chat(name: &str) -> Result<(), String> {
    let out = Command::new("hyprctl").args(["clients", "-j"]).output().map_err(|e| format!("hyprctl: {e}"))?;
    let clients: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let class_of = |c: &Value| c.get("class").and_then(Value::as_str).unwrap_or("").to_string();
    let chat =
        clients.iter().find(|c| Shape::of(&class_of(c)) == Shape::Chat).ok_or("no chat app (Discord) is open")?;
    let class = class_of(chat);
    let focus = format!("hl.dsp.focus({{ window = \"class:^{}$\" }})", regex::escape(&class));
    Command::new("hyprctl").args(["dispatch", &focus]).stdout(Stdio::null()).status().map_err(|e| e.to_string())?;
    pause(300);
    let before = active_window().filter(|w| w.class == class).ok_or(format!("couldn't focus {class}"))?;
    let squash = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    if squash(&before.title).contains(&squash(name)) {
        return Ok(()); // already there
    }
    wtype_args(&["-M", "ctrl", "-k", "k", "-m", "ctrl"]);
    pause(450);
    wtype_text(name);
    pause(1000);
    wtype_args(&["-k", "Return"]);
    pause(800);
    let after = active_window().unwrap_or_default();
    if after.address == before.address && after.title != before.title {
        return Ok(());
    }
    wtype_args(&["-k", "Escape"]); // switcher still open: close it, type nothing
    Err(format!("couldn't find \"{name}\" in {class}"))
}

fn wtype_args(args: &[&str]) -> bool {
    Command::new("wtype").args(args).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Turns streamed lines into what gets typed: drops a preamble the model adds anyway, markdown
/// headings and rules, and (outside chats) bold markers; collapses blank lines.
struct Cleaner {
    preamble: Regex,
    heading: Regex,
    shape: Shape,
    seen_text: bool,
    in_think: bool,
}

impl Cleaner {
    fn new(shape: Shape) -> Cleaner {
        Cleaner {
            preamble: Regex::new(
                r"(?i)^(sure|of course|certainly|absolutely|okay|ok|alright|here'?s|here is|here you go|visst|absolut|självklart|här (är|kommer)|好的|当然)\b.{0,120}[:!]$",
            )
            .unwrap(),
            heading: Regex::new(r"^#{1,6}\s+").unwrap(),
            shape,
            seen_text: false,
            in_think: false,
        }
    }

    /// One finished line of model output → the line to type (None = skip it; Some("") = paragraph break).
    fn line(&mut self, raw: &str) -> Option<String> {
        let mut l = raw.trim_end().to_string();
        if self.in_think {
            let end = l.find("</think>")?;
            self.in_think = false;
            l = l[end + "</think>".len()..].to_string();
        }
        if let Some(start) = l.find("<think>") {
            match l[start..].find("</think>") {
                Some(end) => l.replace_range(start..start + end + "</think>".len(), ""),
                None => {
                    self.in_think = true;
                    l.truncate(start);
                }
            }
        }
        let t = l.trim();
        if t.is_empty() {
            return self.seen_text.then(String::new);
        }
        if !self.seen_text && self.preamble.is_match(t) {
            return None;
        }
        if matches!(t, "---" | "***" | "___") {
            return None;
        }
        let mut s = self.heading.replace(t, "").to_string();
        if self.shape != Shape::Chat {
            s = s.replace("**", "").replace("__", "");
        }
        if !self.seen_text {
            s = s.trim_matches(|c| c == '"' || c == '“' || c == '”').to_string();
        }
        self.seen_text = true;
        Some(s)
    }
}

/// Cut `s` to at most `room` characters, at the end of a sentence if there is one, else at a space.
fn cut(s: &str, room: usize) -> String {
    if s.chars().count() <= room {
        return s.to_string();
    }
    let head: String = s.chars().take(room).collect();
    let at =
        head.rfind(['.', '!', '?', '。', '！', '？']).map(|i| i + head[i..].chars().next().map_or(1, char::len_utf8));
    match at.filter(|&i| i > head.len() / 3) {
        Some(i) => head[..i].to_string(),
        None => head.rfind(' ').map_or(head.clone(), |i| head[..i].to_string()),
    }
}

/// `target` with address "print" = test mode: print to stdout, type nothing, no card or memory.
fn run_worker(cfg: &Config, brief: &str, target: Option<Window>, send: bool) -> i32 {
    let f = Fmt(&cfg.notify);
    let Some(target) = target else {
        eprintln!("write: the window moved before I started");
        return 1;
    };
    let shape = Shape::of(&target.class);
    let print = target.address == "print";
    let mut card = Card::new("writing…");
    card.lines.push(f.plain(&format!("“{brief}”")));
    card.lines.push(f.color("dim", &format!("into {}", target.class)));
    if !print {
        out::card(cfg, &card);
    }

    let mut user = format!(
        "Brief: {brief}\nIt is typed into: {} (window \"{}\"), which is {}.",
        target.class,
        target.title,
        shape.hint()
    );
    let said = std::env::var("CONSENSUS_REQUEST").unwrap_or_default();
    if !said.trim().is_empty() && said.trim() != brief {
        let lang = std::env::var("CONSENSUS_LANG").unwrap_or_default();
        let lang = lang.split(',').next().unwrap_or("").trim();
        user.push_str(&format!("\nThe user's own words were: «{}».", said.trim()));
        if let Some(name) = asked_language(&said) {
            user.push_str(&format!("\nLanguage: {name} (they asked for it): write the text in {name}."));
        } else if !lang.is_empty() {
            let name = crate::council::language_name(lang);
            user.push_str(&format!(
                "\nLanguage: {name}. They spoke {name}, so write the text in {name} unless their words ask for \
                 another language."
            ));
        }
    }
    let history = memory::recent(cfg);
    if !history.trim().is_empty() {
        user.push_str(&format!(
            "\n\nRecent conversation with the user's voice assistant, in case the brief refers to it \
             (\"a longer one\", \"reply to that\"):\n{}",
            history.trim_end()
        ));
    }

    let mut full = String::new(); // everything the model wrote (cleaned), for the clipboard / memory
    let mut typed = 0usize; // characters typed so far
    let mut breaks = 0usize; // paragraph breaks waiting for the next line
    let mut moved = false;
    let mut capped = false;
    let mut type_ok = true;
    let max = shape.max_chars();
    let mut cleaner = Cleaner::new(shape);

    let mut emit = |line: Option<String>| -> bool {
        let Some(line) = line else { return true };
        if line.is_empty() {
            breaks += 1;
            return true;
        }
        let sep = if full.is_empty() {
            String::new()
        } else if shape.one_line() {
            " ".into()
        } else {
            // a blank line between prose paragraphs even if the model used one newline; poems/lists keep theirs
            let prose = |l: &str| l.chars().count() > 100;
            let last = full.rsplit('\n').next().unwrap_or("");
            "\n".repeat(if breaks > 0 || (prose(last) && prose(&line)) { 2 } else { 1 })
        };
        breaks = 0;
        if capped {
            return false;
        }
        let room = max.saturating_sub(full.chars().count() + sep.chars().count());
        let piece = cut(&line, room);
        capped = piece.chars().count() < line.chars().count();
        if piece.is_empty() {
            return false;
        }
        full.push_str(&sep);
        full.push_str(&piece);
        if print {
            print!("{sep}{piece}");
            let _ = std::io::stdout().flush();
            return !capped;
        }
        if moved || !type_ok {
            return !capped;
        }
        if active_window().map(|w| w.address) != Some(target.address.clone()) {
            moved = true;
            return !capped;
        }
        for ch in sep.chars() {
            type_ok &=
                if ch == '\n' { wtype_args(&["-M", "shift", "-k", "Return", "-m", "shift"]) } else { wtype_text(" ") };
        }
        type_ok &= wtype_text(&piece);
        typed += sep.chars().count() + piece.chars().count();
        !capped
    };

    let res = stream(cfg, &user, shape.max_tokens(), |raw| emit(cleaner.line(raw)));
    if let Err(e) = &res {
        eprintln!("write: {e}");
    }
    if print {
        println!("\n--- {:?}, {} characters{}", shape, full.chars().count(), if capped { ", capped" } else { "" });
        return i32::from(res.is_err());
    }
    let _ = std::fs::remove_file(pid_file());

    let words = full.split_whitespace().count();
    let mut done = Card::new(if moved {
        "focus moved: stopped typing"
    } else if res.is_err() && full.is_empty() {
        "writing failed"
    } else {
        "written"
    });
    done.lines.push(f.color("dim", &format!("“{brief}”")));
    let mut sent = false;
    if full.is_empty() {
        done.lines.push(f.color("deny", &res.err().unwrap_or_else(|| "the model wrote nothing".into())));
        done.critical = true;
    } else if moved || !type_ok {
        let copied =
            Command::new("wl-copy").arg("--").arg(&full).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        done.lines.push(f.color(
            "deny",
            &format!(
                "{} of {} characters typed into {}; {}",
                typed,
                full.chars().count(),
                target.class,
                if copied { "the whole text is on the clipboard" } else { "copying it failed too" }
            ),
        ));
        done.critical = true;
    } else {
        if send && active_window().map(|w| w.address) == Some(target.address.clone()) {
            sent = wtype_args(&["-k", "Return"]);
        }
        let mut line = format!("{words} words into {}", target.class);
        if capped {
            line.push_str(&format!(" (cut at {max} characters)"));
        }
        if sent {
            line.push_str(", sent");
        }
        done.lines.push(f.color("approve", &line));
        if let Err(e) = &res {
            done.lines.push(f.color("deny", &format!("stopped early: {e}")));
        }
    }
    out::card(cfg, &done);
    if !full.is_empty() {
        let excerpt: String = full.chars().take(600).collect();
        let more = if full.chars().count() > 600 { "…" } else { "" };
        memory::add(
            cfg,
            &format!("(write) {brief}"),
            &format!(
                "({} {}{}) {excerpt}{more}",
                if moved || !type_ok { "focus moved, on the clipboard instead of" } else { "typed into" },
                target.class,
                if sent { ", sent" } else { "" }
            ),
        );
    }
    i32::from(full.is_empty() || moved)
}

/// `[write] model`, or `busy_model` while a `busy_when` process runs, else the planner's model.
fn writer_model(cfg: &Config) -> String {
    let w = &cfg.write;
    let busy = !w.busy_model.is_empty()
        && !w.busy_when.is_empty()
        && Command::new("pgrep").arg(&w.busy_when).stdout(Stdio::null()).status().is_ok_and(|s| s.success());
    if busy {
        w.busy_model.clone()
    } else if !w.model.is_empty() {
        w.model.clone()
    } else {
        cfg.planner.model.clone()
    }
}

/// Streams a plain-text chat completion; `on_line` gets each finished line and returns false to stop.
fn stream(cfg: &Config, user: &str, max_tokens: u32, mut on_line: impl FnMut(&str) -> bool) -> Result<(), String> {
    let ep = &cfg.endpoint;
    let model = &writer_model(cfg);
    let mut body = json!({
        "model": model,
        "messages": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}],
        "temperature": 0.6,
        "max_tokens": max_tokens,
        "stream": true,
    });
    for (k, v) in &ep.extra {
        body[k] = serde_json::to_value(v).map_err(|e| e.to_string())?;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(ep.timeout.max(180))))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.post(format!("{}/v1/chat/completions", ep.url.trim_end_matches('/')));
    if !ep.api_key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {}", ep.api_key));
    }
    let resp = req.send_json(&body).map_err(|e| format!("chat ({model}): {e}"))?;
    let status = resp.status().as_u16();
    let mut reader = BufReader::new(resp.into_body().into_reader());
    if !(200..300).contains(&status) {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut reader, &mut text);
        return Err(format!("chat ({model}): HTTP {status}: {}", text.chars().take(200).collect::<String>()));
    }
    let mut pending = String::new();
    let mut event = String::new();
    loop {
        event.clear();
        if reader.read_line(&mut event).map_err(|e| format!("chat ({model}): {e}"))? == 0 {
            break;
        }
        let Some(data) = event.trim().strip_prefix("data:").map(str::trim) else { continue };
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) else { continue };
        pending.push_str(delta);
        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            if !on_line(line.trim_end_matches('\n')) {
                return Ok(());
            }
        }
    }
    if !pending.trim().is_empty() {
        on_line(&pending);
    }
    Ok(())
}

/// A language the request explicitly asks the text to be in ("in English", "på engelska", "用英文").
fn asked_language(request: &str) -> Option<&'static str> {
    let r = request.to_lowercase();
    let table: &[(&[&str], &str)] = &[
        (&["in english", "på engelska", "用英文", "用英语", "英文写", "英语写", "翻译成英"], "English"),
        (&["in swedish", "på svenska", "用瑞典语", "瑞典语写", "瑞典文"], "Swedish"),
        (&["in chinese", "in mandarin", "på kinesiska", "用中文", "中文写"], "Chinese"),
        (&["in german", "på tyska"], "German"),
        (&["in spanish", "på spanska"], "Spanish"),
        (&["in french", "på franska"], "French"),
        (&["in japanese", "på japanska", "用日语"], "Japanese"),
        (&["in finnish", "på finska"], "Finnish"),
    ];
    // the last one named wins ("translate my Swedish into English" is not this function's job anyway)
    table
        .iter()
        .filter_map(|(cues, name)| cues.iter().filter_map(|c| r.rfind(c)).max().map(|at| (at, *name)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, name)| name)
}

/// Words that say a brief DESCRIBES text (a kind of text, or what it is about) instead of being it.
const DESCRIBING: &[&str] = &[
    "essay",
    "poem",
    "story",
    "message",
    "reply",
    "answer",
    "joke",
    "text",
    "letter",
    "summary",
    "roast",
    "song",
    "rap",
    "speech",
    "email",
    "mail",
    "post",
    "review",
    "list",
    "paragraph",
    "sentence",
    "words",
    "something",
    "anything",
    "about",
    "saying",
    "says",
    "that",
    "telling",
    "explaining",
    "why",
    "how",
    "uppsats",
    "dikt",
    "berättelse",
    "historia",
    "meddelande",
    "svar",
    "skämt",
    "brev",
    "sammanfattning",
    "låt",
    "tal",
    "något",
    "någonting",
    "om",
    "att",
    "säger",
    "varför",
    "hur",
    "诗",
    "文章",
    "故事",
    "消息",
    "回复",
    "笑话",
    "信",
    "关于",
    "说",
    "作文",
    "总结",
];

/// A brief that is just the words to type ("hej på dig", "lol", "gg ez"): no word says what kind of
/// text it is or what it is about.
fn is_dictation(brief: &str) -> bool {
    let low = brief.to_lowercase();
    let cjk = low.chars().any(|c| matches!(c as u32, 0x4e00..=0x9fff));
    let words: Vec<String> = low
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches('\'').to_string())
        .collect();
    let describes = DESCRIBING.iter().any(|d| {
        if d.chars().any(|c| c as u32 >= 0x4e00) {
            low.contains(d)
        } else {
            words.iter().any(|w| w == d || (d.len() > 4 && w.starts_with(d)))
        }
    });
    !describes && !cjk && words.len() <= 12
}

/// Words that ask to send / press Enter, in the languages the assistant hears.
const SEND_WORDS: &[&str] = &["send", "skicka", "sänd", "post", "submit", "enter", "return", "发送", "发出去"];

/// Fixes the typing commands the planner writes, before rules and votes see them:
/// - a ' inside a single-quoted text ('I'm late') is re-quoted ('I'\''m late') instead of breaking the shell;
/// - `consensus write ... && wtype -k Return` becomes `--send` (Enter would hit the chat while the worker
///   is still writing);
/// - Enter / --send is dropped when the request never asked to send: a message sent by mistake can't be unsent.
pub fn repair(request: &str, command: &str) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"^(?P<head>(?:~/\.local/bin/)?consensus write(?: --send| --to '(?:[^']|'\\'')*')* -- |wtype -- |wl-copy -- )'(?P<body>.*)'(?P<tail>(?: && wtype -k Return)?)$",
        )
        .unwrap()
    });
    let cmd = command.trim();
    let Some(m) = re.captures(cmd) else { return command.to_string() };
    let body = &m["body"];
    if ["' && ", "' || ", "'; ", "' | ", "' &&", "' ;"].iter().any(|s| body.contains(s)) {
        return command.to_string(); // several quoted parts: not ours to touch
    }
    // '\'' is the right escape; a bare \' inside single quotes (models write both) would type a backslash
    let text = body.replace(r"'\''", "'").replace(r"\'", "'");
    let mut head = m["head"].to_string();
    let mut tail = m["tail"].to_string();
    if head.contains("consensus write") && !tail.is_empty() {
        tail.clear();
        if !head.contains("--send") {
            head = head.replacen("consensus write", "consensus write --send", 1);
        }
    }
    let low = request.to_lowercase();
    if !SEND_WORDS.iter().any(|w| low.contains(w)) {
        tail.clear();
        head = head.replacen(" --send", "", 1);
    }
    format!("{head}'{}'{tail}", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(shape: Shape, text: &str) -> Vec<String> {
        let mut c = Cleaner::new(shape);
        text.lines().filter_map(|l| c.line(l)).collect()
    }

    #[test]
    fn drops_preamble_and_headings() {
        let got = clean(
            Shape::Document,
            "Sure! Here's an essay about Overwatch:\n\n# Overwatch\n\nOverwatch is a **hero** shooter.\n\nIt is fun.",
        );
        assert_eq!(got, vec!["Overwatch", "", "Overwatch is a hero shooter.", "", "It is fun."]);
    }

    #[test]
    fn keeps_markdown_in_chats_and_think_blocks_go() {
        let got = clean(Shape::Chat, "<think>\nhmm\n</think>\nHär är **kul**.");
        assert_eq!(got, vec!["Här är **kul**."]);
    }

    #[test]
    fn swedish_preamble_and_first_line_quotes() {
        let got = clean(Shape::Chat, "Visst, här är ett meddelande:\n“Jag är sen, sorry!”");
        assert_eq!(got, vec!["Jag är sen, sorry!"]);
    }

    #[test]
    fn cut_prefers_sentence_ends() {
        assert_eq!(cut("One two. Three four five six", 15), "One two.");
        assert_eq!(cut("short", 15), "short");
        assert_eq!(cut("no sentence end here at all", 12), "no sentence");
    }

    #[test]
    fn repair_quotes_send_and_enter() {
        assert_eq!(repair("type I'm late", "wtype -- 'I'm late'"), r"wtype -- 'I'\''m late'");
        assert_eq!(repair("type I'm late", r"wtype -- 'I'\''m late'"), r"wtype -- 'I'\''m late'");
        assert_eq!(repair("type don't", r"wtype -- 'don\'\''t'"), r"wtype -- 'don'\''t'");
        assert_eq!(
            repair("write lol and send it", "consensus write -- 'lol' && wtype -k Return"),
            "consensus write --send -- 'lol'"
        );
        assert_eq!(
            repair("skriv ett meddelande till mamma", "consensus write --send -- 'ett meddelande'"),
            "consensus write -- 'ett meddelande'"
        );
        assert_eq!(repair("type hi", "wtype -- 'hi' && wtype -k Return"), "wtype -- 'hi'");
        assert_eq!(
            repair("skriv hej och skicka", "wtype -- 'hej' && wtype -k Return"),
            "wtype -- 'hej' && wtype -k Return"
        );
        assert_eq!(
            repair(
                "skriv till 25 man och skicka en uppsats",
                "consensus write --to '25 man' -- 'en uppsats' && wtype -k Return"
            ),
            "consensus write --send --to '25 man' -- 'en uppsats'"
        );
        let two = "wtype -- 'a' && wtype -k Return && wtype -- 'b'";
        assert_eq!(repair("type a send type b", two), two);
        assert_eq!(repair("volume up", "wpctl set-volume @DEFAULT_SINK@ 5%+"), "wpctl set-volume @DEFAULT_SINK@ 5%+");
    }

    #[test]
    fn explicit_language_wins() {
        assert_eq!(asked_language("skriv en dikt om katter på engelska"), Some("English"));
        assert_eq!(asked_language("用英文写一首诗"), Some("English"));
        assert_eq!(asked_language("write a poem in Swedish"), Some("Swedish"));
        assert_eq!(asked_language("write a poem about cats"), None);
    }

    #[test]
    fn dictation_vs_description() {
        for b in ["hej på dig", "lol", "gg ez", "I'm on my way", "hello there"] {
            assert!(is_dictation(b), "{b}");
        }
        for b in [
            "en uppsats om spermier",
            "a poem about cats",
            "that I'm late",
            "a roast of my friend Ebi",
            "ett meddelande till mamma",
            "something funny",
            "一首关于猫的诗",
            "essays on war",
        ] {
            assert!(!is_dictation(b), "{b}");
        }
    }

    #[test]
    fn shapes() {
        assert_eq!(Shape::of("discord"), Shape::Chat);
        assert_eq!(Shape::of("Alacritty"), Shape::Terminal);
        assert_eq!(Shape::of("cs2"), Shape::Game);
        assert_eq!(Shape::of("firefox"), Shape::Document);
    }
}
