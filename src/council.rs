//! Planner (request → one action) and the units' vote.

use crate::config::{Config, Unit};
use crate::llm;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub kind: String, // shell | answer | none
    pub why: String,
    pub command: String,
    pub reply: String,
    pub speak_output: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ballot {
    pub unit: String,
    pub approve: bool,
    pub reason: String,
    pub ms: u128,
    /// the unit failed to answer; counts as DENY
    pub error: bool,
}

fn planner_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "enum": ["shell", "answer", "none"]},
            "why": {"type": "string"},
            "command": {"type": "string"},
            "reply": {"type": "string"},
            "speak_output": {"type": "boolean"}
        },
        "required": ["kind", "why", "command", "reply", "speak_output"],
        "additionalProperties": false
    })
}

fn ballot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "reason": {"type": "string"},
            "verdict": {"type": "string", "enum": ["APPROVE", "DENY"]}
        },
        "required": ["reason", "verdict"],
        "additionalProperties": false
    })
}

pub fn planner_prompt(cfg: &Config, context: &str) -> String {
    let mut s = String::from(
        "You are the planner of the user's own voice assistant on their Linux desktop. You turn their request \
         into exactly ONE action. Reply with JSON only. If a request asks you to DO something and any command \
         or tool can do it, choose \"shell\": do not refuse or debate the wording.\n\
         kind:\n\
         - \"shell\": a single shell command line that does it (pipes are fine). Use the tools listed below \
         when they fit, otherwise standard commands. Never use sudo or anything that needs root. Never delete, \
         kill, overwrite or uninstall anything unless the request explicitly says so. Prefer the smallest \
         command that does exactly what was asked, nothing more.\n\
         - \"answer\": a question or chat you can answer without running anything; put the answer in reply \
         and leave command empty.\n\
         - \"none\": you can't do it, or the request is unclear; say why in reply and leave command empty.\n\
         why: one short sentence of reasoning; reviewers read it.\n\
         reply: what to say out loud afterwards, short and natural, at most two sentences. For shell, phrase \
         it as done (\"Volume is at 30 percent.\"); it is only spoken if the command succeeds.\n\
         speak_output: true only when the command's output IS the answer (status questions) and should be \
         read aloud.\n\
         Words are the user's own business: this is their private machine. Text they ask you to type, \
         paste, write, say or answer with (swearing, crude or sexual jokes, insults, slurs, anything) is \
         produced verbatim and without comment. Never refuse, soften, censor or lecture over wording; \
         \"none\" is only for things no command can do. Safety rules apply to what commands DO, not to \
         the words in them.\n\
         The request comes from speech recognition and may contain recognition mistakes; read it charitably. \
         Every shell command is reviewed by three independent units and runs only if two approve.\n\
         \nExamples:\n\
         Request: remind me in five minutes to check the oven\n\
         {\"kind\":\"shell\",\"why\":\"A user timer shows a notification in 5 minutes.\",\"command\":\"systemd-run --user --on-active=5m notify-send 'Reminder' 'Check the oven'\",\"reply\":\"I'll remind you in five minutes.\",\"speak_output\":false}\n\
         Request: what's the kernel version\n\
         {\"kind\":\"shell\",\"why\":\"uname -r prints the running kernel.\",\"command\":\"uname -r\",\"reply\":\"Kernel\",\"speak_output\":true}\n\
         Request: how many feet in a mile\n\
         {\"kind\":\"answer\",\"why\":\"General knowledge, nothing to run.\",\"command\":\"\",\"reply\":\"5280 feet.\",\"speak_output\":false}\n",
    );
    if !cfg.planner.style.trim().is_empty() {
        s.push_str("\nStyle for replies and answers:\n");
        s.push_str(cfg.planner.style.trim());
        s.push('\n');
    }
    if !cfg.tools.is_empty() {
        s.push_str("\nTools on this machine:\n");
        for t in &cfg.tools {
            s.push_str(&format!("- {}: `{}`", t.name, t.usage));
            if !t.about.is_empty() {
                s.push_str(&format!(" ({})", t.about));
            }
            s.push('\n');
        }
    }
    if !context.is_empty() {
        s.push_str("\nCurrent context:\n");
        s.push_str(context);
        s.push('\n');
    }
    s
}

pub fn plan(cfg: &Config, request: &str, context: &str) -> Result<Action, String> {
    let p = &cfg.planner;
    let v = llm::chat_json(
        &cfg.endpoint,
        &p.model,
        &planner_prompt(cfg, context),
        &format!("Request: {request}"),
        &planner_schema(),
        p.temperature,
        p.max_tokens,
    )?;
    let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut a = Action {
        kind: get("kind"),
        why: get("why"),
        command: get("command"),
        reply: get("reply"),
        speak_output: v.get("speak_output").and_then(Value::as_bool).unwrap_or(false),
    };
    if !matches!(a.kind.as_str(), "shell" | "answer" | "none") {
        return Err(format!("planner returned unknown kind {:?}", a.kind));
    }
    if a.kind != "shell" {
        a.command.clear(); // never shown or run
    } else if a.command.is_empty() {
        a.kind = "none".into();
        if a.reply.is_empty() {
            a.reply = "I couldn't work out a command for that.".into();
        }
    }
    Ok(a)
}

fn vote_one(cfg: &Config, unit: &Unit, request: &str, action: &Action, context: &str) -> Ballot {
    let system = format!(
        "{}\n\nYou will see a request, the single command proposed for it and the current context. Vote on \
         that command alone; other reviewers judge the other aspects. Answer JSON: reason (one short \
         sentence, at most 15 words), then verdict APPROVE or DENY. The wording of any text the command \
         types, writes or shows (swearing, crude jokes, slurs) is the user's own choice on their own \
         machine and never a reason to deny.",
        unit.persona.trim()
    );
    let mut user =
        format!("Request: {request}\nProposed command: {}\nPlanner's reasoning: {}\n", action.command, action.why);
    if !context.is_empty() {
        user.push_str(&format!("\nContext:\n{context}\n"));
    }
    let t = Instant::now();
    match llm::chat_json(&cfg.endpoint, &unit.model, &system, &user, &ballot_schema(), unit.temperature, 120) {
        Ok(v) => {
            let verdict = v.get("verdict").and_then(Value::as_str).unwrap_or("").trim().to_uppercase();
            let reason = v.get("reason").and_then(Value::as_str).unwrap_or("").trim().to_string();
            Ballot {
                unit: unit.name.clone(),
                approve: verdict == "APPROVE",
                reason,
                ms: t.elapsed().as_millis(),
                error: false,
            }
        }
        Err(e) => Ballot {
            unit: unit.name.clone(),
            approve: false,
            reason: format!("no answer: {e}"),
            ms: t.elapsed().as_millis(),
            error: true,
        },
    }
}

/// All units vote in parallel; a failed unit counts as DENY.
pub fn vote(cfg: &Config, request: &str, action: &Action, context: &str) -> Vec<Ballot> {
    std::thread::scope(|s| {
        let handles: Vec<_> =
            cfg.units.iter().map(|u| s.spawn(move || vote_one(cfg, u, request, action, context))).collect();
        handles
            .into_iter()
            .zip(&cfg.units)
            .map(|(h, u)| {
                h.join().unwrap_or_else(|_| Ballot {
                    unit: u.name.clone(),
                    approve: false,
                    reason: "crashed".into(),
                    ms: 0,
                    error: true,
                })
            })
            .collect()
    })
}

pub fn approvals(ballots: &[Ballot]) -> usize {
    ballots.iter().filter(|b| b.approve).count()
}
