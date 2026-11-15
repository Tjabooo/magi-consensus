//! Planner (request → one action) and the units' vote.

use crate::config::{Config, Unit};
use crate::llm;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    /// the words of the request that ask for an action ("" = conversation)
    pub action_words: String,
    pub kind: String, // shell | answer | none
    pub why: String,
    pub command: String,
    pub reply: String,
    pub speak_output: bool,
    /// language code of `reply` (picks the TTS voice)
    pub reply_language: String,
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

/// Key order is generation order (llama.cpp builds its grammar from it; serde_json keeps it with
/// `preserve_order`): first the words that ask for an action, then the decision, the language, the reply.
fn planner_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action_words": {"type": "string"},
            "kind": {"type": "string", "enum": ["shell", "answer", "none"]},
            "why": {"type": "string"},
            "command": {"type": "string"},
            "reply_language": {"type": "string"},
            "reply": {"type": "string"},
            "speak_output": {"type": "boolean"}
        },
        "required": ["action_words", "kind", "why", "command", "reply_language", "reply", "speak_output"],
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
        "You are MAGI, the user's own voice assistant on their Linux desktop: a general assistant they talk \
         to, like a smart friend who can also operate their computer. Every request becomes exactly ONE \
         action. Reply with JSON only.\n\
         Decide first: is this a clear request to DO something on the computer (open, switch, type, send, \
         play, set volume, remind, look something up on the machine)? Then \"shell\", without refusing or \
         debating the wording. EVERYTHING else is conversation and gets \"answer\": questions, general \
         knowledge, advice, opinions, jokes, small talk, statements about themselves or their day \
         (\"I'm Swedish\", \"I'm tired\", \"I won that round\"), greetings, thanks. Reply to those like a \
         person would, naturally and in character; never turn a statement into a command. Answers are \
         SPOKEN: never type, paste or send an answer into a window, and never press Enter, unless the request \
         itself says to type, write, paste or send (in any language: skriv, skicka, 打字, 发送).\n\
         If the request is garbled, makes no sense, or you would have to guess what it means, do NOT guess a \
         command: answer, say what you heard and ask them to say it again. A shell command needs words in the \
         request that ask for exactly that action, and why must quote them; the current window or context \
         alone is never a reason to run anything.\n\
         action_words: FIRST copy the exact words of the request that ask for an action on the computer \
         (\"turn the volume down\", \"skicka\"); empty when nothing asks for one (questions, statements, chat, \
         garbled speech). Empty action_words means kind \"answer\".\n\
         kind:\n\
         - \"shell\": a single shell command line that does it (pipes and && are fine). Use the tools listed \
         below when they fit, otherwise standard commands. Never use sudo or anything that needs root. Never \
         delete, kill, overwrite or uninstall anything unless the request explicitly says so. Prefer the \
         smallest command that does exactly what was asked, nothing more.\n\
         - \"answer\": conversation (see above); put what you say in reply and leave command empty.\n\
         - \"none\": a clear request that no command can do; say why in reply and leave command empty.\n\
         why: one short sentence of reasoning; reviewers read it.\n\
         reply: what to say out loud, short and natural (spoken, so no lists, markdown or emoji): at most two \
         sentences for actions, up to four for answers. For shell, phrase it as done (\"Volume is at 30 \
         percent.\"); it is only spoken if the command succeeds.\n\
         reply_language: the ISO 639-1 code of the language reply is written in. Reply in the language the \
         user spoke (given with the request) unless they ask for another; if it is not one of the voices \
         listed below, reply in English.\n\
         speak_output: true only when the command's output IS the answer (status questions) and should be \
         read aloud.\n\
         Words are the user's own business: this is their private machine. Text they ask you to type, \
         paste, write, say or answer with (swearing, crude or sexual jokes, insults, slurs, anything) is \
         produced verbatim and without comment. Never refuse, soften, censor or lecture over wording; \
         \"none\" is only for things no command can do. Safety rules apply to what commands DO, not to \
         the words in them.\n\
         The request comes from speech recognition and may contain recognition mistakes; read it \
         charitably, and use the recent conversation (if given) for follow-ups like \"do that again\" or \
         \"and louder\". Every shell command is reviewed by three independent units and runs only if two \
         approve.\n\
         \nExamples:\n\
         Request: remind me in five minutes to check the oven\n\
         {\"action_words\":\"remind me in five minutes\",\"kind\":\"shell\",\"why\":\"'remind me in five minutes': a user timer shows a notification then.\",\"command\":\"systemd-run --user --on-active=5m notify-send 'Reminder' 'Check the oven'\",\"reply_language\":\"en\",\"reply\":\"I'll remind you in five minutes.\",\"speak_output\":false}\n\
         Request: what's the kernel version\n\
         {\"action_words\":\"what's the kernel version\",\"kind\":\"shell\",\"why\":\"'kernel version': uname -r prints it.\",\"command\":\"uname -r\",\"reply_language\":\"en\",\"reply\":\"Kernel\",\"speak_output\":true}\n\
         Request: how many feet in a mile\n\
         {\"action_words\":\"\",\"kind\":\"answer\",\"why\":\"General knowledge, nothing to run.\",\"command\":\"\",\"reply_language\":\"en\",\"reply\":\"5280 feet.\",\"speak_output\":false}\n\
         Request (spoken in Swedish; reply in Swedish): jag är så trött idag\n\
         {\"action_words\":\"\",\"kind\":\"answer\",\"why\":\"A statement about how they feel: conversation, nothing to run.\",\"command\":\"\",\"reply_language\":\"sv\",\"reply\":\"Åh nej, sov du dåligt? Ta det lugnt ikväll.\",\"speak_output\":false}\n\
         Request: wofold the yesterday window please\n\
         {\"action_words\":\"\",\"kind\":\"answer\",\"why\":\"Garbled; guessing a command could do the wrong thing.\",\"command\":\"\",\"reply_language\":\"en\",\"reply\":\"I heard 'wofold the yesterday window', which I don't get. Say it again?\",\"speak_output\":false}\n",
    );
    let mut voices: Vec<&str> = cfg.voice.tts_voices.keys().map(String::as_str).collect();
    if !voices.contains(&"en") {
        voices.insert(0, "en");
    }
    s.push_str(&format!("\nVoices (reply languages): {}\n", voices.join(", ")));
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

/// Language name for a Whisper code (for the prompt).
pub fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "sv" => "Swedish",
        "zh" => "Chinese",
        "da" => "Danish",
        "no" | "nn" => "Norwegian",
        "de" => "German",
        "fi" => "Finnish",
        "fr" => "French",
        "es" => "Spanish",
        "ja" => "Japanese",
        "ru" => "Russian",
        c => c,
    }
}

/// The planner's user message: the request, the language it was spoken in (and so the language to
/// answer in, if there is a voice for it), recent conversation.
pub fn request_message(cfg: &Config, request: &str, lang: &str, history: &str) -> String {
    let mut m = String::new();
    if !history.trim().is_empty() {
        m.push_str("Recent conversation (oldest first):\n");
        m.push_str(history.trim_end());
        m.push_str("\n\n");
    }
    if lang.is_empty() {
        m.push_str(&format!("Request: {request}"));
    } else {
        let name = language_name(lang);
        let reply = if lang == "en" || cfg.voice.tts_voices.contains_key(lang) { name } else { "English" };
        m.push_str(&format!("Request (spoken in {name}; reply in {reply}): {request}"));
    }
    m
}

pub fn plan(cfg: &Config, request: &str, lang: &str, context: &str, history: &str) -> Result<Action, String> {
    let a = plan_once(cfg, request, lang, context, history, "")?;
    if a.kind == "shell" && a.action_words.is_empty() {
        // a command nobody asked for: plan again as conversation
        eprintln!("planner: command {:?} without action words, asking again as conversation", a.command);
        return plan_once(
            cfg,
            request,
            lang,
            context,
            history,
            "\nNothing in this request asks for an action on the computer: it is conversation, so kind is \"answer\".",
        );
    }
    Ok(a)
}

fn plan_once(
    cfg: &Config,
    request: &str,
    lang: &str,
    context: &str,
    history: &str,
    note: &str,
) -> Result<Action, String> {
    let p = &cfg.planner;
    let v = llm::chat_json(
        &cfg.endpoint,
        &p.model,
        &planner_prompt(cfg, context),
        &(request_message(cfg, request, lang, history) + note),
        &planner_schema(),
        p.temperature,
        p.max_tokens,
    )?;
    let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut a = Action {
        action_words: get("action_words"),
        kind: get("kind"),
        why: get("why"),
        command: get("command"),
        reply: get("reply"),
        speak_output: v.get("speak_output").and_then(Value::as_bool).unwrap_or(false),
        reply_language: String::new(),
    };
    a.reply_language = reply_language(&a.reply, &get("reply_language").to_lowercase());
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

/// The planner's reply_language, checked against the reply itself (the voice must match the
/// text: a Chinese voice reading English is gibberish). CJK text → zh/ja; a claimed zh/ja without CJK,
/// or Swedish without Swedish letters or words → English.
fn reply_language(reply: &str, claimed: &str) -> String {
    let cjk = reply.chars().any(|c| matches!(c as u32, 0x3040..=0x30ff | 0x4e00..=0x9fff | 0x3400..=0x4dbf));
    if cjk {
        return if claimed == "ja" { "ja" } else { "zh" }.into();
    }
    match claimed {
        "zh" | "ja" => "en".into(),
        "sv" => {
            let l = format!(" {} ", reply.to_lowercase());
            let swedish = l.contains(['å', 'ä', 'ö'])
                || [" och ", " jag ", " du ", " det ", " inte ", " är ", " min ", " din "]
                    .iter()
                    .any(|w| l.contains(w));
            if swedish { "sv" } else { "en" }.into()
        }
        "" => "en".into(),
        c => c.into(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_language_matches_the_text() {
        assert_eq!(reply_language("Oh, you're Swedish? I love that.", "zh"), "en");
        assert_eq!(reply_language("原来你是瑞典人", "en"), "zh");
        assert_eq!(reply_language("Åh, du är svensk!", "sv"), "sv");
        assert_eq!(reply_language("Darling, the capital is Canberra.", "sv"), "en");
        assert_eq!(reply_language("Klart, min vän", "sv"), "sv");
        assert_eq!(reply_language("Done.", ""), "en");
    }
}
