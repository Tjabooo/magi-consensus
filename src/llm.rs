//! OpenAI-compatible client: chat completions with a JSON schema, and Whisper transcription.

use crate::config::Endpoint;
use serde_json::{json, Value};
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

pub fn transcribe(ep: &Endpoint, wav: &[u8], language: &str, prompt: &str) -> Result<String, String> {
    let boundary = format!("magiconsensus{:x}", std::process::id());
    let mut body = Vec::with_capacity(wav.len() + 1024);
    let mut field = |k: &str, v: &str| {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    };
    field("model", &ep.transcribe_model);
    field("response_format", "json");
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
    let v = finish(req.send(&body[..]), "transcribe")?;
    Ok(v.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string())
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
}
