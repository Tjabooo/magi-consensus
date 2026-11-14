//! Config: built-in defaults (src/defaults.toml) with the user's file merged over them.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const DEFAULTS: &str = include_str!("defaults.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub endpoint: Endpoint,
    pub planner: Planner,
    pub vote: Vote,
    pub units: Vec<Unit>,
    pub run: Run,
    pub rules: Rules,
    pub context: Context,
    pub tools: Vec<Tool>,
    pub notify: Notify,
    pub voice: Voice,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    pub url: String,
    pub api_key: String,
    pub timeout: u64,
    pub transcribe_model: String,
    pub extra: toml::Table,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Planner {
    pub model: String,
    pub temperature: f64,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Vote {
    pub needed: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Unit {
    pub name: String,
    pub model: String,
    pub temperature: f64,
    pub persona: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub shell: Vec<String>,
    pub timeout: u64,
    pub max_output: usize,
    pub cwd: String,
    pub path_prepend: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rules {
    pub never: Vec<String>,
    pub destructive: Vec<String>,
    pub destructive_prefixes: Vec<String>,
    pub destructive_patterns: Vec<String>,
    pub opaque: Vec<String>,
    pub opaque_patterns: Vec<String>,
    pub redirect_is_destructive: bool,
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Context {
    pub commands: Vec<String>,
    pub max_chars: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    pub usage: String,
    #[serde(default)]
    pub about: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Notify {
    pub enabled: bool,
    pub command: String,
    pub app_name: String,
    pub markup: bool,
    pub timeout_ms: i64,
    pub colors: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub record: Vec<String>,
    pub max_seconds: u64,
    pub min_rms: f64,
    pub language: String,
    pub prompt: String,
    pub tts: String,
    pub speak_output_chars: usize,
}

impl Notify {
    pub fn color(&self, key: &str) -> &str {
        self.colors.get(key).map(String::as_str).unwrap_or("#ffffff")
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~` and `~/x` expand to $HOME.
pub fn expand(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MAGI_CONSENSUS_CONFIG") {
        return PathBuf::from(p);
    }
    xdg("XDG_CONFIG_HOME", ".config").join("magi-consensus/config.toml")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("magi-consensus")
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("magi-consensus")
}

/// Tables merge key by key; anything else (arrays included) replaces.
fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

pub fn parse(user: Option<&str>) -> Result<Config, String> {
    let mut base: toml::Value = toml::from_str(DEFAULTS).map_err(|e| format!("built-in defaults: {e}"))?;
    if let Some(text) = user {
        let over: toml::Value = toml::from_str(text).map_err(|e| format!("{}: {e}", config_path().display()))?;
        merge(&mut base, over);
    }
    let cfg: Config = base.try_into().map_err(|e| format!("{}: {e}", config_path().display()))?;
    if cfg.units.is_empty() {
        return Err("config: at least one [[units]] entry is needed".into());
    }
    if cfg.vote.needed == 0 || cfg.vote.needed > cfg.units.len() {
        return Err(format!("config: vote.needed must be 1..={}", cfg.units.len()));
    }
    if cfg.run.shell.is_empty() {
        return Err("config: run.shell is empty".into());
    }
    Ok(cfg)
}

pub fn load() -> Result<Config, String> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(Some(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => parse(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = parse(None).unwrap();
        assert_eq!(c.units.len(), 3);
        assert_eq!(c.vote.needed, 2);
        assert!(c.rules.never.iter().any(|w| w == "sudo"));
    }

    #[test]
    fn user_overrides_merge() {
        let c = parse(Some("[endpoint]\nurl = \"http://x:1\"\n[vote]\nneeded = 3\n")).unwrap();
        assert_eq!(c.endpoint.url, "http://x:1");
        assert_eq!(c.endpoint.timeout, 120, "untouched keys keep defaults");
        assert_eq!(c.vote.needed, 3);
    }

    #[test]
    fn needed_out_of_range_is_an_error() {
        assert!(parse(Some("[vote]\nneeded = 4\n")).is_err());
    }
}
