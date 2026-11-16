//! OpenAI-compatible client: chat completions with a JSON schema, and Whisper transcription.

use crate::config::Endpoint;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

fn agent(timeout: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn url(ep: &Endpoint, path: &str) -> String {
    format!("{}{}", ep.url.trim_end_matches('/'), path)
}

fn finish(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>, what: &str) -> Result<Value, String> {
    let mut resp = resp.map_err(|e| format!("{what}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().map_err(|e| format!("{what}: reading reply: {e}"))?;
    if !(200..300).contains(&status) {
        let short: String = text.chars().take(300).collect();
        return Err(format!("{what}: HTTP {status}: {short}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("{what}: bad JSON reply: {e}"))
}

/// First `{...}` object in a model reply (tolerates code fences and stray text).
pub fn extract_json(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        if v.is_object() {
            return Some(v);
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(&text[start..=end]).ok().filter(Value::is_object)
}

/// One chat call constrained to `schema`; returns the parsed object.
pub fn chat_json(
    ep: &Endpoint,
    model: &str,
    system: &str,
    user: &str,
    schema: &Value,
    temperature: f64,
    max_tokens: u32,
) -> Result<Value, String> {
    let mut body = json!({
        "model": model,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        "temperature": temperature,
        "max_tokens": max_tokens,
        "response_format": {"type": "json_schema", "json_schema": {"name": "reply", "strict": true, "schema": schema}},
    });
    for (k, v) in &ep.extra {
        body[k] = serde_json::to_value(v).map_err(|e| e.to_string())?;
    }
    let mut req = agent(ep.timeout).post(url(ep, "/v1/chat/completions"));
    if !ep.api_key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {}", ep.api_key));
    }
    let out = finish(req.send_json(&body), &format!("chat ({model})"))?;
    let content = out.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("");
    extract_json(content).ok_or_else(|| {
        let short: String = content.chars().take(200).collect();
        format!("chat ({model}): reply is not JSON: {short:?}")
    })
}

/// What Whisper heard: the text and the language it was in (ISO code, "" if the server doesn't say).
#[derive(Debug, Clone, Default)]
pub struct Heard {
    pub text: String,
    pub lang: String,
    /// every language heard, most likely first (a second one counts from 15 %: mixed speech)
    pub langs: Vec<String>,
}

/// A second language at least this likely means the speech mixed two languages.
const SECOND_LANGUAGE: f64 = 0.15;

/// Transcribe a WAV. `language` empty = auto-detect; a detected language that is a key of `remap`
/// is transcribed again as the value (Swedish heard as Danish/Norwegian → "sv").
pub fn transcribe(
    ep: &Endpoint,
    wav: &[u8],
    language: &str,
    prompt: &str,
    remap: &BTreeMap<String, String>,
) -> Result<Heard, String> {
    let v = transcribe_once(ep, wav, language, prompt)?;
    let mut heard = heard_from(&v, language);
    if language.is_empty() {
        if let Some(to) = remap.get(&heard.lang).filter(|to| !to.is_empty() && **to != heard.lang) {
            eprintln!("heard {:?} as {}, transcribing again as {to}", heard.text, heard.lang);
            let v = transcribe_once(ep, wav, to, prompt)?;
            let mut again = heard_from(&v, to);
            again.langs = heard.langs.clone();
            heard = again;
        }
        // the remap applies to every language heard ("da,en" → "sv,en")
        let mut langs: Vec<String> = Vec::new();
        for l in heard.langs.iter().map(|l| remap.get(l).filter(|t| !t.is_empty()).unwrap_or(l)) {
            if !langs.contains(l) {
                langs.push(l.clone());
            }
        }
        heard.langs = langs;
    }
    Ok(heard)
}

fn heard_from(v: &Value, language: &str) -> Heard {
    let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut langs = detected_all(v);
    let lang = if !language.is_empty() { language.to_string() } else { langs.first().cloned().unwrap_or_default() };
    langs.retain(|l| *l != lang);
    if !lang.is_empty() {
        langs.insert(0, lang.clone());
    }
    langs.truncate(2);
    Heard { text, lang, langs }
}

/// Languages of a verbose_json reply, most likely first: the top one, plus a second one when it is
/// at least SECOND_LANGUAGE likely (whisper.cpp: language_probabilities).
fn detected_all(v: &Value) -> Vec<String> {
    if let Some(p) = v.get("language_probabilities").and_then(Value::as_object) {
        let mut ps: Vec<(&String, f64)> = p.iter().filter_map(|(k, x)| x.as_f64().map(|x| (k, x))).collect();
        ps.sort_by(|a, b| b.1.total_cmp(&a.1));
        return ps
            .iter()
            .enumerate()
            .filter(|(i, (_, x))| *i == 0 || (*i == 1 && *x >= SECOND_LANGUAGE))
            .map(|(_, (k, _))| (*k).clone())
            .collect();
    }
    let d = detected(v);
    if d.is_empty() {
        vec![]
    } else {
        vec![d]
    }
}

/// The most likely language code of a verbose_json reply (whisper.cpp: language_probabilities).
fn detected(v: &Value) -> String {
    if let Some(p) = v.get("language_probabilities").and_then(Value::as_object) {
        if let Some((k, _)) =
            p.iter().filter_map(|(k, x)| x.as_f64().map(|x| (k, x))).max_by(|a, b| a.1.total_cmp(&b.1))
        {
            return k.clone();
        }
    }
    // OpenAI-style servers give only a language name
    match v.get("language").and_then(Value::as_str).unwrap_or("").to_lowercase().as_str() {
        "english" | "en" => "en",
        "swedish" | "sv" => "sv",
        "danish" | "da" => "da",
        "norwegian" | "no" => "no",
        "nynorsk" | "nn" => "nn",
        "chinese" | "zh" => "zh",
        "german" | "de" => "de",
        _ => "",
    }
    .to_string()
}

fn transcribe_once(ep: &Endpoint, wav: &[u8], language: &str, prompt: &str) -> Result<Value, String> {
    let boundary = format!("magiconsensus{:x}", std::process::id());
    let mut body = Vec::with_capacity(wav.len() + 1024);
    let mut field = |k: &str, v: &str| {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    };
    field("model", &ep.transcribe_model);
    field("response_format", "verbose_json");
    if !language.is_empty() {
        field("language", language);
    }
    if !prompt.is_empty() {
        field("prompt", prompt);
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes());
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let mut req = agent(ep.timeout)
        .post(url(ep, "/v1/audio/transcriptions"))
        .header("Content-Type", &format!("multipart/form-data; boundary={boundary}"));
    if !ep.api_key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {}", ep.api_key));
    }
    finish(req.send(&body[..]), "transcribe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_noise() {
        assert_eq!(extract_json("```json\n{\"a\":1}\n```").unwrap()["a"], 1);
        assert_eq!(extract_json("{\"a\":2}").unwrap()["a"], 2);
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn detected_language() {
        let v = json!({"language": "swedish", "language_probabilities": {"sv": 0.3, "da": 0.6, "no": 0.1}});
        assert_eq!(detected(&v), "da");
        assert_eq!(detected(&json!({"language": "Swedish"})), "sv");
        assert_eq!(detected(&json!({})), "");
        assert_eq!(heard_from(&v, "sv").lang, "sv");
        assert_eq!(heard_from(&v, "").langs, vec!["da", "sv"]);
        let mixed = json!({"language_probabilities": {"zh": 0.69, "en": 0.30, "ja": 0.01}});
        assert_eq!(heard_from(&mixed, "").langs, vec!["zh", "en"]);
        let one = json!({"language_probabilities": {"sv": 0.88, "no": 0.05}});
        assert_eq!(heard_from(&one, "").langs, vec!["sv"]);
        assert_eq!(heard_from(&mixed, "sv").langs, vec!["sv", "zh"]);
    }
}
