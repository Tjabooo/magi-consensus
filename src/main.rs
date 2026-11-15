//! consensus: three LLM units vote on every action; it runs only with enough approvals.

mod config;
mod council;
mod ears;
mod exec;
mod fast;
mod llm;
mod out;
mod ptt;
mod rules;

use config::Config;
use council::Action;
use out::{Card, Fmt};
use serde_json::json;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELP: &str = "\
consensus: a local assistant where three LLM units vote on every action

USAGE
  consensus ask [--dry-run] [--quiet] [--vote] TEXT...
                                                your own [[commands]] run at once; anything else
                                                is planned, voted on, and run if approved
  consensus ptt start|stop|toggle|cancel        push to talk (bind start on press, stop on release)
  consensus ears run|on|off|toggle|status       always-on listening for the wake word (run = daemon)
  consensus ears test FILE.wav                  where the wake word is heard in a 16 kHz mono WAV
  consensus ears debug on|off                   save every burst of speech (last 40) to tune the wake word
  consensus commands                            the fast phrases, as configured
  consensus check COMMAND...                    hard rules only, no models
  consensus hear FILE.wav                       transcribe only (tests the speech endpoint)
  consensus log [N]                             last N decisions (default 10)
  consensus config                              config path + effective settings

  --dry-run   vote but never run the command
  --quiet     no voice (the card still shows)
  --vote      skip the fast path: always plan and vote

Config: $XDG_CONFIG_HOME/magi-consensus/config.toml (MAGI_CONSENSUS_CONFIG overrides)
Log:    $XDG_STATE_HOME/magi-consensus/votes.jsonl";

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

struct Opts {
    dry_run: bool,
    quiet: bool,
    source: &'static str,
    vote: bool,
}

/// Every request goes here: your own phrases first, the council for everything else.
fn handle(cfg: &Config, request: &str, o: &Opts) -> i32 {
    if !o.vote {
        if let Some(steps) = fast::plan(cfg, request) {
            return run_fast(cfg, request, &steps, o);
        }
    }
    decide(cfg, request, o)
}

fn run_fast(cfg: &Config, request: &str, steps: &[fast::Step], o: &Opts) -> i32 {
    let f = Fmt(&cfg.notify);
    let t0 = Instant::now();
    println!("request : {request}");
    for s in steps {
        println!("fast    : {}  →  {}", s.phrase, cfg.commands[s.command].run);
    }
    let mut record = json!({"t": now_ms(), "source": o.source, "request": request, "dry_run": o.dry_run,
        "fast": steps.iter().map(|s| json!({"phrase": s.phrase, "run": cfg.commands[s.command].run})).collect::<Vec<_>>()});
    if o.dry_run {
        return 0;
    }
    let rep = fast::run(cfg, steps);
    record["result"] = json!(if rep.ok { "fast" } else { "fast-failed" });
    record["lines"] = json!(rep.lines);
    record["ms"] = json!(t0.elapsed().as_millis());
    out::log(&record);
    let mut c = Card::new(if rep.ok { "done" } else { "failed" });
    c.lines.push(f.color("dim", &format!("“{request}”")));
    c.lines.extend(rep.lines.iter().map(|l| f.color(if l.starts_with('✓') { "approve" } else { "deny" }, l)));
    c.critical = !rep.ok;
    out::card_for(cfg, &c, if rep.ok { 2500 } else { cfg.notify.timeout_ms });
    for l in &rep.lines {
        println!("{l}");
    }
    if !o.quiet {
        if rep.speech.is_empty() {
            if !cfg.fast.done_sound.trim().is_empty() {
                let _ = std::process::Command::new("sh")
                    .args(["-c", &config::expand_cmd(&cfg.fast.done_sound)])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        } else {
            out::speak(cfg, &rep.speech.join(" "));
        }
    }
    if rep.ok {
        0
    } else {
        1
    }
}

fn cmd_ears(cfg: &Config, what: &str, rest: &[String]) -> i32 {
    let note = |t: &str| {
        let mut c = Card::new(t);
        c.lines.push(Fmt(&cfg.notify).color("dim", "always-on listening"));
        out::card_for(cfg, &c, 2500);
    };
    match what {
        "run" => {
            let c2 = cfg.clone();
            match ears::run(cfg, move |text: &str| {
                let o = Opts { dry_run: false, quiet: false, source: "ears", vote: false };
                handle(&c2, text, &o);
            }) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("ears: {e}");
                    1
                }
            }
        }
        "on" | "off" | "toggle" => {
            let pause = match what {
                "on" => false,
                "off" => true,
                _ => !ears::paused(),
            };
            match ears::set_paused(pause) {
                Ok(()) => {
                    note(if pause { "ears off" } else { "ears on" });
                    println!("{}", if pause { "paused" } else { "listening" });
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "status" => {
            println!("{}", if ears::paused() { "paused" } else { "listening" });
            0
        }
        "debug" => {
            let on = rest.first().map(String::as_str) != Some("off");
            let f = ears::debug_flag();
            let r = if on {
                std::fs::create_dir_all(f.parent().unwrap()).and_then(|_| std::fs::write(&f, ""))
            } else {
                std::fs::remove_file(&f)
                    .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
            };
            match r {
                Ok(()) => {
                    println!("debug {}: {}", if on { "on" } else { "off" }, ears::debug_dir().display());
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "test" => {
            let Some(path) = rest.first() else {
                eprintln!("usage: consensus ears test FILE.wav");
                return 64;
            };
            let pcm = match std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| ears::read_wav(&b)) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{path}: {e}");
                    return 1;
                }
            };
            let dir = config::expand(&cfg.ears.model_dir);
            if let Ok(lines) = ears::keyword_lines(&dir, &cfg.ears.keywords) {
                for l in lines {
                    eprintln!("keyword: {l}");
                }
            }
            match ears::spot_file(cfg, &pcm) {
                Ok(hits) if hits.is_empty() => {
                    println!("no wake word heard");
                    1
                }
                Ok(hits) => {
                    for (t, k) in hits {
                        println!("{t:6.2} s  {k}");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: consensus ears run|on|off|toggle|status|test FILE.wav|debug on|off");
            64
        }
    }
}

fn cmd_commands(cfg: &Config) -> i32 {
    for c in &cfg.commands {
        println!("{}", c.say.join("  ·  "));
        println!("    → {}", c.run);
    }
    if !cfg.apps.is_empty() {
        println!();
        for a in &cfg.apps {
            let aka = if a.aliases.is_empty() { String::new() } else { format!(" ({})", a.aliases.join(", ")) };
            println!("app {}{aka}", a.name);
        }
    }
    0
}

/// The whole pipeline for one request. Returns the process exit code.
fn decide(cfg: &Config, request: &str, o: &Opts) -> i32 {
    let f = Fmt(&cfg.notify);
    let t0 = Instant::now();
    let mut record = json!({"t": now_ms(), "source": o.source, "request": request, "dry_run": o.dry_run});
    let say = |text: &str| {
        if !o.quiet {
            out::speak(cfg, text);
        }
    };

    let mut thinking = Card::new("deliberating");
    thinking.lines.push(f.plain(&format!("“{request}”")));
    out::card(cfg, &thinking);

    let context = exec::context(cfg);
    let action: Action = match council::plan(cfg, request, &context) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("planner: {e}");
            let mut c = Card::new("planner failed");
            c.lines.push(f.color("deny", &e));
            c.critical = true;
            out::card(cfg, &c);
            record["error"] = json!(format!("planner: {e}"));
            record["ms"] = json!(t0.elapsed().as_millis());
            out::log(&record);
            say("The planner failed.");
            return 1;
        }
    };
    record["action"] = json!(action);
    println!("request : {request}");

    if action.kind != "shell" {
        println!("{:<8}: {}", action.kind, action.reply);
        let mut c = Card::new(if action.kind == "answer" { "answer" } else { "no action" });
        c.lines.push(f.color("dim", &format!("“{request}”")));
        c.lines.push(f.plain(&action.reply));
        out::card(cfg, &c);
        record["result"] = json!(action.kind);
        record["ms"] = json!(t0.elapsed().as_millis());
        out::log(&record);
        say(&action.reply);
        return 0;
    }

    println!("command : {}", action.command);
    println!("why     : {}", action.why);

    // hard rules: no vote
    match rules::check(&cfg.rules, &action.command) {
        Ok(rules::Verdict::Pass) => {}
        Ok(rules::Verdict::Refuse(why)) | Err(why) => {
            println!("REFUSED : {why}");
            let mut c = Card::new("refused by rule");
            c.lines.push(f.command(&action.command));
            c.lines.push(f.bold("deny", &why));
            c.critical = true;
            out::card(cfg, &c);
            record["result"] = json!("refused");
            record["rule"] = json!(why);
            record["ms"] = json!(t0.elapsed().as_millis());
            out::log(&record);
            say("Refused. That breaks a hard rule.");
            return 3;
        }
    }

    let mut voting = Card::new("voting");
    voting.lines.push(f.color("dim", &format!("“{request}”")));
    voting.lines.push(f.command(&action.command));
    out::card(cfg, &voting);

    let ballots = council::vote(cfg, request, &action, &context);
    let yes = council::approvals(&ballots);
    let n = ballots.len();
    let approved = yes >= cfg.vote.needed;
    for b in &ballots {
        println!("{:<9} {:<7} {} ({} ms)", b.unit, if b.approve { "APPROVE" } else { "DENY" }, b.reason, b.ms);
    }
    record["votes"] = json!(ballots);
    record["approved"] = json!(approved);

    let mut c = Card::new(if approved { format!("APPROVED {yes}/{n}") } else { format!("DENIED {yes}/{n}") });
    c.lines.push(f.command(&action.command));
    c.lines.extend(ballots.iter().map(|b| f.ballot(b)));

    if !approved {
        println!("DENIED  : {yes}/{n}");
        out::card(cfg, &c);
        record["result"] = json!("denied");
        record["ms"] = json!(t0.elapsed().as_millis());
        out::log(&record);
        let reason = ballots
            .iter()
            .find(|b| !b.approve && !b.error)
            .map(|b| format!("{} says: {}", title_case(&b.unit), b.reason))
            .unwrap_or_default();
        say(&format!("Denied, {yes} of {n}. {reason}"));
        return 2;
    }

    if o.dry_run {
        println!("APPROVED: {yes}/{n} (dry run, not run)");
        c.lines.push(f.color("dim", "dry run: not executed"));
        out::card(cfg, &c);
        record["result"] = json!("approved-dry-run");
        record["ms"] = json!(t0.elapsed().as_millis());
        out::log(&record);
        say(&format!("Approved, {yes} of {n}. Dry run, so nothing ran."));
        return 0;
    }

    let res = exec::run(&cfg.run, &action.command, Duration::from_secs(cfg.run.timeout));
    println!("APPROVED: {yes}/{n} → exit {:?} in {} ms", res.code, res.ms);
    if !res.stdout.is_empty() {
        println!("{}", res.stdout);
    }
    if !res.stderr.is_empty() {
        eprintln!("{}", res.stderr);
    }
    let spoken = if res.ok() {
        let mut s = action.reply.clone();
        if action.speak_output && !res.stdout.is_empty() {
            let outp: String = res.stdout.chars().take(cfg.voice.speak_output_chars).collect();
            s = if s.is_empty() { outp } else { format!("{s} {outp}") };
        }
        if s.is_empty() {
            "Done.".to_string()
        } else {
            s
        }
    } else if res.timed_out {
        "That timed out.".to_string()
    } else {
        let first = res.stderr.lines().next().unwrap_or("").chars().take(120).collect::<String>();
        format!("That failed. {first}")
    };
    let tail = if res.ok() {
        let first = res.stdout.lines().next().unwrap_or("");
        if first.is_empty() {
            f.color("approve", "done")
        } else {
            f.color("dim", &format!("→ {}", first.chars().take(120).collect::<String>()))
        }
    } else {
        f.color("deny", &format!("failed (exit {:?}): {}", res.code, res.stderr.lines().next().unwrap_or("")))
    };
    c.lines.push(tail);
    c.critical = !res.ok();
    out::card(cfg, &c);
    record["result"] = json!(if res.ok() { "ran" } else { "failed" });
    record["run"] = json!(res);
    record["ms"] = json!(t0.elapsed().as_millis());
    out::log(&record);
    say(&spoken);
    code_of(&res)
}

fn code_of(r: &exec::Outcome) -> i32 {
    if r.ok() {
        0
    } else {
        4
    }
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn cmd_ptt(cfg: &Config, what: &str, o: &mut Opts) -> i32 {
    let f = Fmt(&cfg.notify);
    let stop = |o: &Opts| -> i32 {
        let mut c = Card::new("listening…");
        c.lines.push(f.color("dim", "transcribing"));
        out::card(cfg, &c);
        match ptt::stop(cfg) {
            Ok(Some(text)) => handle(cfg, &text, o),
            Ok(None) => {
                let mut c = Card::new("heard nothing");
                c.lines.push(f.color("dim", "too quiet or empty: nothing to vote on"));
                out::card(cfg, &c);
                0
            }
            Err(e) => {
                let mut c = Card::new("speech failed");
                c.lines.push(f.color("deny", &e));
                c.critical = true;
                out::card(cfg, &c);
                eprintln!("{e}");
                1
            }
        }
    };
    o.source = "voice";
    match what {
        "start" | "toggle" if ptt::recording().is_some() => stop(o),
        "start" | "toggle" => match ptt::start(cfg) {
            Ok(()) => {
                let mut c = Card::new("listening…");
                c.lines.push(f.color("accent", "speak, then release"));
                out::card(cfg, &c);
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        "stop" => {
            if ptt::recording().is_none() {
                return 0; // release after a toggle-stop: nothing to do
            }
            stop(o)
        }
        "cancel" => {
            ptt::cancel();
            0
        }
        _ => {
            eprintln!("usage: consensus ptt start|stop|toggle|cancel");
            64
        }
    }
}

fn cmd_log(n: usize) -> i32 {
    let path = config::state_dir().join("votes.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    for l in &lines[lines.len().saturating_sub(n)..] {
        let v: serde_json::Value = match serde_json::from_str(l) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let votes = v
            .get("votes")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().map(|b| if b["approve"].as_bool() == Some(true) { "✓" } else { "✗" }).collect::<String>())
            .unwrap_or_default();
        println!(
            "{:<17} {:<4} {:<9} {}  {}",
            s("/result"),
            votes,
            s("/action/kind"),
            s("/request"),
            if s("/action/command").is_empty() { String::new() } else { format!("$ {}", s("/action/command")) }
        );
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = args.first().map(String::as_str) else {
        println!("{HELP}");
        std::process::exit(64);
    };
    if matches!(sub, "-h" | "--help" | "help") {
        println!("{HELP}");
        return;
    }
    if matches!(sub, "-V" | "--version") {
        println!("consensus {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(78);
        }
    };
    let mut o = Opts { dry_run: false, quiet: false, source: "text", vote: false };
    let mut rest: Vec<String> = Vec::new();
    for a in &args[1..] {
        match a.as_str() {
            "--dry-run" | "-n" => o.dry_run = true,
            "--quiet" | "-q" => o.quiet = true,
            "--vote" => o.vote = true,
            _ => rest.push(a.clone()),
        }
    }
    let code = match sub {
        "ask" => {
            let text = rest.join(" ");
            if text.trim().is_empty() {
                eprintln!("usage: consensus ask TEXT...");
                64
            } else {
                handle(&cfg, text.trim(), &o)
            }
        }
        "ears" => cmd_ears(&cfg, rest.first().map(String::as_str).unwrap_or("status"), rest.get(1..).unwrap_or(&[])),
        "commands" => cmd_commands(&cfg),
        "ptt" => cmd_ptt(&cfg, rest.first().map(String::as_str).unwrap_or("toggle"), &mut o),
        "check" => match rules::check(&cfg.rules, &rest.join(" ")) {
            Ok(rules::Verdict::Pass) => {
                println!("pass (would go to the vote)");
                0
            }
            Ok(rules::Verdict::Refuse(w)) | Err(w) => {
                println!("refused: {w}");
                3
            }
        },
        "hear" => match rest.first().map(std::fs::read) {
            Some(Ok(wav)) => match llm::transcribe(&cfg.endpoint, &wav, &cfg.voice.language, &cfg.voice.prompt) {
                Ok(t) => {
                    println!("{t}");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            },
            Some(Err(e)) => {
                eprintln!("{e}");
                66
            }
            None => {
                eprintln!("usage: consensus hear FILE.wav");
                64
            }
        },
        "log" => cmd_log(rest.first().and_then(|n| n.parse().ok()).unwrap_or(10)),
        "config" => {
            println!("# {}", config::config_path().display());
            println!(
                "endpoint {} · planner {} · units {} · needed {}",
                cfg.endpoint.url,
                cfg.planner.model,
                cfg.units.iter().map(|u| format!("{}={}", u.name, u.model)).collect::<Vec<_>>().join(" "),
                cfg.vote.needed
            );
            println!(
                "tools {} · context commands {} · allowlist {} · tts {}",
                cfg.tools.len(),
                cfg.context.commands.len(),
                cfg.rules.allow.len(),
                if cfg.voice.tts.is_empty() { "off" } else { "on" }
            );
            0
        }
        _ => {
            eprintln!("unknown command {sub:?}\n\n{HELP}");
            64
        }
    };
    std::process::exit(code);
}
