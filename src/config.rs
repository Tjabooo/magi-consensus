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
    pub fast: Fast,
    #[serde(default)]
    pub apps: Vec<App>,
    #[serde(default)]
    pub commands: Vec<FastCommand>,
    pub ears: Ears,
    #[serde(default)]
    pub write: Write,
}

/// `consensus write`: composing text from a brief.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Write {
    /// model for composing ("" = the planner's model)
    #[serde(default)]
    pub model: String,
    /// used instead of `model` while a process matching this regex runs (a game that needs the VRAM)
    #[serde(default)]
    pub busy_model: String,
    #[serde(default)]
    pub busy_when: String,
}

/// Fast path: your own phrases run at once, no model, no vote.
#[derive(Debug, Clone, Deserialize)]
pub struct Fast {
    pub enabled: bool,
    pub step_delay_ms: u64,
    pub launch_wait_s: u64,
    pub wake_words: Vec<String>,
    pub lead_fillers: Vec<String>,
    pub tail_fillers: Vec<String>,
    /// run instead of Hyprland's fullscreen ("{address}" = the window); empty = built in
    pub fullscreen_cmd: String,
    /// played (in the background) after fast commands that say nothing
    pub done_sound: String,
    // `own_commands_only` is read from the raw TOML in parse(): your [[commands]] replace the
    // built-in ones instead of coming before them
}

/// An app the fast path can name: `{app}` in a phrase.
#[derive(Debug, Clone, Deserialize)]
pub struct App {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// regex on the window class
    #[serde(default)]
    pub class: String,
    /// regex on the window title
    #[serde(default)]
    pub title: String,
    /// substring of a process command line below the window's pid (tells gamescope games apart)
    #[serde(default)]
    pub process: String,
    /// how to start it when it isn't open
    #[serde(default)]
    pub launch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FastCommand {
    pub say: Vec<String>,
    pub run: String,
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub speak_output: bool,
}

/// Always-on listening: a keyword spotter waits for the wake word on the CPU, then Whisper hears
/// the request.
#[derive(Debug, Clone, Deserialize)]
pub struct Ears {
    pub model_dir: String,
    pub keywords: Vec<String>,
    pub threshold: f32,
    pub score: f32,
    pub threads: i32,
    pub record: Vec<String>,
    pub preroll_ms: u64,
    pub silence_ms: u64,
    pub start_timeout_ms: u64,
    pub max_seconds: u64,
    pub speech_factor: f64,
    pub wake_sound: String,
    pub require_wake_word: bool,
    /// false: wake-word requests that aren't your own phrases run the planner's command after the
    /// hard rules, without a council vote (push to talk and `ask` still vote)
    #[serde(default)]
    pub vote: bool,
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
    /// Extra instructions on tone and personality, appended to the planner prompt.
    #[serde(default)]
    pub style: String,
    /// how many recent exchanges the planner sees (0 = no memory)
    pub memory_turns: usize,
    /// exchanges older than this are forgotten
    pub memory_minutes: u64,
    /// words that make a request lasting; only then may the planner add a standing note
    pub note_cues: Vec<String>,
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
    /// Regexes: a command matching one has its output kept out of the log and the card (clipboard reads).
    #[serde(default)]
    pub private: Vec<String>,
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
    pub hold_key: String,
    pub min_rms: f64,
    pub language: String,
    /// detected language → language to transcribe again in (e.g. da = "sv")
    #[serde(default)]
    pub language_remap: BTreeMap<String, String>,
    /// language code → TTS pipeline for replies in that language; others use `tts`
    #[serde(default)]
    pub tts_voices: BTreeMap<String, String>,
    pub prompt: String,
    pub tts: String,
    pub speak_output_chars: usize,
}

impl Notify {
    pub fn color(&self, key: &str) -> &str {
        self.colors.get(key).map(String::as_str).unwrap_or("#ffffff")
    }
}

/// `~/` and `$HOME` in a shell command line.
pub fn expand_cmd(c: &str) -> String {
    let h = home().display().to_string();
    let c = c.replace("$HOME", &h).replace("${HOME}", &h);
    let c = if let Some(r) = c.strip_prefix("~/") { format!("{h}/{r}") } else { c };
    c.replace(" ~/", &format!(" {h}/"))
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
        let mut over: toml::Value = toml::from_str(text).map_err(|e| format!("{}: {e}", config_path().display()))?;
        // your [[commands]] come first and keep the built-in ones, unless fast.own_commands_only
        let only = over.get("fast").and_then(|f| f.get("own_commands_only")).and_then(|v| v.as_bool()).unwrap_or(false);
        if let (Some(toml::Value::Array(mine)), false) = (over.get("commands").cloned(), only) {
            if let Some(toml::Value::Array(builtin)) = base.get("commands").cloned() {
                over.as_table_mut().unwrap().insert("commands".into(), toml::Value::Array([mine, builtin].concat()));
            }
        }
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
    fn own_commands_come_first() {
        let base = parse(None).unwrap().commands.len();
        let c = parse(Some("[[commands]]\nsay = [\"clip that\"]\nrun = \"clip\"\n")).unwrap();
        assert_eq!(c.commands.len(), base + 1);
        assert_eq!(c.commands[0].run, "clip");
        let c = parse(Some("[fast]\nown_commands_only = true\n[[commands]]\nsay = [\"x\"]\nrun = \"y\"\n")).unwrap();
        assert_eq!(c.commands.len(), 1);
    }

    #[test]
    fn needed_out_of_range_is_an_error() {
        assert!(parse(Some("[vote]\nneeded = 4\n")).is_err());
    }
}
