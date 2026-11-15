//! Always-on listening. A small keyword-spotting model (sherpa-onnx, CPU, one thread) waits for
//! the wake word; only then is the request recorded, until you stop talking, and sent to Whisper.
//! Nothing leaves the machine and the GPU is only touched after a wake.

use crate::config::{self, Config};
use crate::{llm, out};
use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RATE: usize = 16000;
const CHUNK: usize = 1600; // 100 ms
const FRAME: usize = 480; // 30 ms for the speech/silence decision

pub fn paused_flag() -> PathBuf {
    config::state_dir().join("ears-paused")
}

/// `consensus ears debug on`: every burst of speech is saved (last 40) with whether it woke.
pub fn debug_flag() -> PathBuf {
    config::state_dir().join("ears-debug")
}

pub fn debug_dir() -> PathBuf {
    config::state_dir().join("debug")
}

fn save_burst(pcm: &[i16], woke: bool) {
    let dir = debug_dir();
    let _ = std::fs::create_dir_all(&dir);
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let peak = pcm.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
    let name = format!("{ms}_{}_peak{peak}.wav", if woke { "WOKE" } else { "nowake" });
    let _ = std::fs::write(dir.join(&name), wav(pcm));
    eprintln!(
        "ears: speech {:.1} s, peak {peak}, rms {:.0}: {} → debug/{name}",
        pcm.len() as f64 / RATE as f64,
        rms(pcm),
        if woke { "woke" } else { "no wake" }
    );
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    let extra = files.len().saturating_sub(40);
    for f in &files[..extra] {
        let _ = std::fs::remove_file(f);
    }
}

pub fn paused() -> bool {
    paused_flag().exists()
}

pub fn set_paused(p: bool) -> std::io::Result<()> {
    let f = paused_flag();
    if p {
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(f, "")
    } else {
        match std::fs::remove_file(f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn pick(dir: &Path, prefix: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix) && n.ends_with(".onnx"))
        .collect();
    // int8 first: same accuracy for keyword spotting, a third of the CPU
    names.sort_by_key(|n| (!n.contains(".int8."), n.clone()));
    names.first().map(|n| dir.join(n).display().to_string())
}

pub fn spotter(cfg: &Config) -> Result<KeywordSpotter, String> {
    let dir = config::expand(&cfg.ears.model_dir);
    let need = |p: &str| pick(&dir, p).ok_or_else(|| format!("{}: no {p}*.onnx", dir.display()));
    let mut k = KeywordSpotterConfig::default();
    k.model_config.transducer.encoder = Some(need("encoder")?);
    k.model_config.transducer.decoder = Some(need("decoder")?);
    k.model_config.transducer.joiner = Some(need("joiner")?);
    k.model_config.tokens = Some(dir.join("tokens.txt").display().to_string());
    k.model_config.num_threads = cfg.ears.threads.max(1);
    k.model_config.provider = Some("cpu".into());
    k.keywords_threshold = cfg.ears.threshold;
    k.keywords_score = cfg.ears.score;
    let lines = keyword_lines(&dir, &cfg.ears.keywords)?;
    if lines.is_empty() {
        return Err("ears.keywords is empty".into());
    }
    k.keywords_buf = Some(lines.join("\n"));
    KeywordSpotter::create(&k).ok_or_else(|| "sherpa-onnx couldn't load the keyword model (see stderr)".to_string())
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let (mut r, mut shift) = (0u64, 0);
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        r |= ((c & 0x7f) as u64) << shift;
        if c < 0x80 {
            return Some(r);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Protobuf fields of one message: (field, wire type, payload) — enough for sentencepiece.
fn proto_fields(b: &[u8]) -> Vec<(u64, u64, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let Some(key) = varint(b, &mut i) else { break };
        let (field, wt) = (key >> 3, key & 7);
        let start = i;
        let payload: &[u8] = match wt {
            0 => {
                if varint(b, &mut i).is_none() {
                    break;
                }
                &b[start..i]
            }
            1 => {
                i += 8;
                b.get(start..i).unwrap_or(&[])
            }
            2 => {
                let Some(n) = varint(b, &mut i) else { break };
                let s = i;
                i += n as usize;
                match b.get(s..i) {
                    Some(p) => p,
                    None => break,
                }
            }
            5 => {
                i += 4;
                b.get(start..i).unwrap_or(&[])
            }
            _ => break,
        };
        out.push((field, wt, payload));
    }
    out
}

/// piece → score from a sentencepiece `bpe.model` (ModelProto.pieces = field 1).
pub fn bpe_pieces(model: &[u8]) -> HashMap<String, f32> {
    let mut m = HashMap::new();
    for (f, wt, p) in proto_fields(model) {
        if f != 1 || wt != 2 {
            continue;
        }
        let (mut piece, mut score) = (None, 0.0f32);
        for (g, gt, q) in proto_fields(p) {
            match (g, gt) {
                (1, 2) => piece = std::str::from_utf8(q).ok().map(str::to_string),
                (2, 5) if q.len() == 4 => score = f32::from_le_bytes([q[0], q[1], q[2], q[3]]),
                _ => {}
            }
        }
        if let Some(pc) = piece {
            m.insert(pc, score);
        }
    }
    m
}

/// Sentencepiece BPE: start from characters, merge the best-scoring neighbour pair until none is
/// left. Words are upper-cased with "▁" in front (the model's spelling).
pub fn bpe_encode(pieces: &HashMap<String, f32>, text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let w = format!("▁{}", word.to_uppercase());
        let mut sym: Vec<String> = w.chars().map(|c| c.to_string()).collect();
        // "▁" alone isn't always a piece: glue it to the first letter when that pair exists
        loop {
            let mut best: Option<(usize, f32)> = None;
            for i in 0..sym.len().saturating_sub(1) {
                if let Some(&sc) = pieces.get(&format!("{}{}", sym[i], sym[i + 1])) {
                    if best.is_none_or(|(_, b)| sc > b) {
                        best = Some((i, sc));
                    }
                }
            }
            let Some((i, _)) = best else { break };
            let merged = format!("{}{}", sym[i], sym[i + 1]);
            sym.splice(i..i + 2, [merged]);
        }
        if sym.iter().any(|s| !pieces.contains_key(s)) {
            return None;
        }
        out.extend(sym);
    }
    (!out.is_empty()).then_some(out)
}

/// sherpa-onnx wants keywords as model tokens: "▁MA G I @MAGI".
pub fn keyword_lines(dir: &Path, words: &[String]) -> Result<Vec<String>, String> {
    let model = dir.join("bpe.model");
    let pieces = std::fs::read(&model).map(|b| bpe_pieces(&b)).map_err(|e| format!("{}: {e}", model.display()))?;
    let mut lines = Vec::new();
    for w in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        if w.contains('▁') {
            lines.push(w.to_string()); // already tokens
            continue;
        }
        let toks = bpe_encode(&pieces, w)
            .ok_or_else(|| format!("ears.keywords: can't spell \"{w}\" with the model's tokens"))?;
        lines.push(format!("{} @{}", toks.join(" "), w.to_uppercase().replace(' ', "_")));
    }
    Ok(lines)
}

fn f32s(pcm: &[i16]) -> Vec<f32> {
    pcm.iter().map(|&s| s as f32 / 32768.0).collect()
}

fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    (pcm.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / pcm.len() as f64).sqrt()
}

pub fn wav(pcm: &[i16]) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&(RATE as u32).to_le_bytes());
    w.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// PCM of a 16 kHz mono s16 WAV (the format pw-record and the tests write).
pub fn read_wav(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" {
        return Err("not a WAV file".into());
    }
    // walk the chunks to "data" (pw-record adds a LIST chunk)
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"fmt " {
            let rate = u32::from_le_bytes(bytes[i + 12..i + 16].try_into().unwrap());
            let ch = u16::from_le_bytes(bytes[i + 10..i + 12].try_into().unwrap());
            if rate != RATE as u32 || ch != 1 {
                return Err(format!("need 16 kHz mono, got {rate} Hz × {ch}"));
            }
        }
        if id == b"data" {
            let end = (i + 8 + len).min(bytes.len());
            return Ok(bytes[i + 8..end].as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect());
        }
        i += 8 + len + (len & 1);
    }
    Err("no data chunk".into())
}

/// Keyword spotting over a whole file: (seconds, keyword) of every hit.
pub fn spot_file(cfg: &Config, pcm: &[i16]) -> Result<Vec<(f32, String)>, String> {
    let k = spotter(cfg)?;
    let s = k.create_stream();
    let mut hits = Vec::new();
    for (n, c) in pcm.chunks(CHUNK).enumerate() {
        s.accept_waveform(RATE as i32, &f32s(c));
        while k.is_ready(&s) {
            k.decode(&s);
            if let Some(r) = k.get_result(&s).filter(|r| !r.keyword.is_empty()) {
                hits.push(((n * CHUNK) as f32 / RATE as f32, r.keyword.clone()));
                k.reset(&s);
            }
        }
    }
    Ok(hits)
}

/// Where the request starts in a transcript: after the first wake word near the start.
/// None = Whisper heard no wake word, so this was probably a false wake.
pub fn after_wake(cfg: &Config, transcript: &str) -> Option<String> {
    let norm = crate::fast::normalize(transcript);
    let words: Vec<&str> = norm.split_whitespace().filter(|w| *w != ",").collect();
    let singles: Vec<String> = cfg.fast.wake_words.iter().filter(|w| !w.contains(' ')).cloned().collect();
    let pos = words.iter().take(6).position(|w| singles.iter().any(|s| s == w))?;
    Some(words[pos + 1..].join(" "))
}

fn sh_bg(cmd: &str) {
    if cmd.trim().is_empty() {
        return;
    }
    let _ = Command::new("sh")
        .args(["-c", &config::expand_cmd(cmd)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

enum State {
    Idle,
    /// recording a request; `spoke` = speech has started
    Hearing {
        pcm: Vec<i16>,
        started: Instant,
        spoke: bool,
        quiet_ms: u64,
    },
}

/// The daemon. `handle` gets each transcript (wake word already removed).
pub fn run(cfg: &Config, handle: impl Fn(&str) + Send + Sync + 'static) -> Result<(), String> {
    let k = spotter(cfg)?;
    let mut stream = k.create_stream();
    if cfg.ears.record.is_empty() {
        return Err("ears.record is empty".into());
    }
    let mut child = Command::new(&cfg.ears.record[0])
        .args(&cfg.ears.record[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("can't start {}: {e}", cfg.ears.record[0]))?;
    let mut mic = child.stdout.take().ok_or("no recorder output")?;
    // pw-record without --raw writes Sun AU to stdout: ".snd" header, big-endian samples
    let mut head = [0u8; 4];
    mic.read_exact(&mut head).map_err(|e| format!("recorder stopped: {e}"))?;
    let mut carry: Vec<u8> = Vec::new();
    let big_endian = &head == b".snd";
    if big_endian {
        let mut off = [0u8; 4];
        mic.read_exact(&mut off).map_err(|e| format!("recorder stopped: {e}"))?;
        let skip = (u32::from_be_bytes(off) as usize).saturating_sub(8);
        let mut rest = vec![0u8; skip];
        mic.read_exact(&mut rest).map_err(|e| format!("recorder stopped: {e}"))?;
        eprintln!("ears: recorder writes Sun AU (add --raw to ears.record); decoding big-endian");
    } else {
        carry.extend_from_slice(&head);
    }
    eprintln!("ears: listening for {:?} (threshold {}, {})", cfg.ears.keywords, cfg.ears.threshold, cfg.ears.model_dir);

    let handle = Arc::new(handle);
    let busy = Arc::new(AtomicBool::new(false));
    let preroll = (cfg.ears.preroll_ms as usize * RATE / 1000).max(CHUNK);
    let mut ring: VecDeque<i16> = VecDeque::with_capacity(preroll + CHUNK);
    // noise floor: slow average of quiet 30 ms frames
    let mut floor = 200.0f64;
    let mut state = State::Idle;
    let mut buf = vec![0u8; CHUNK * 2];
    let mut was_paused = paused();
    let mut last_pause_check = Instant::now();
    let mut debug = debug_flag().exists();
    // debug: the current burst of speech, its trailing quiet, whether it woke
    let mut burst: Option<(Vec<i16>, u64, bool)> = None;
    let mut recent: VecDeque<i16> = VecDeque::with_capacity(RATE);

    loop {
        let n = carry.len();
        buf[..n].copy_from_slice(&carry);
        carry.clear();
        mic.read_exact(&mut buf[n..]).map_err(|e| format!("recorder stopped: {e}"))?;
        let pcm: Vec<i16> = buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| if big_endian { i16::from_be_bytes(*c) } else { i16::from_le_bytes(*c) })
            .collect();

        for f in pcm.chunks(FRAME) {
            let r = rms(f);
            if r < floor * 1.5 {
                floor = floor * 0.98 + r * 0.02;
            }
        }
        floor = floor.clamp(20.0, 3000.0);

        if last_pause_check.elapsed() > Duration::from_millis(500) {
            last_pause_check = Instant::now();
            let d = debug_flag().exists();
            if d != debug {
                eprintln!("ears: debug {}", if d { "on (saving speech to debug/)" } else { "off" });
                debug = d;
            }
            let p = paused();
            if p != was_paused {
                eprintln!("ears: {}", if p { "paused" } else { "listening" });
                was_paused = p;
            }
        }

        if debug {
            let loud = rms(&pcm) > (floor * cfg.ears.speech_factor).max(cfg.voice.min_rms);
            burst = match burst.take() {
                None if loud => {
                    let mut b: Vec<i16> = recent.iter().copied().collect();
                    b.extend_from_slice(&pcm);
                    Some((b, 0, false))
                }
                None => None,
                Some((mut b, q, w)) => {
                    b.extend_from_slice(&pcm);
                    let q = if loud { 0 } else { q + 100 };
                    if q >= 600 || b.len() > 8 * RATE {
                        save_burst(&b, w);
                        None
                    } else {
                        Some((b, q, w))
                    }
                }
            };
        }
        recent.extend(pcm.iter().copied());
        while recent.len() > RATE / 2 {
            recent.pop_front();
        }

        state = match state {
            State::Idle => {
                ring.extend(pcm.iter().copied());
                while ring.len() > preroll {
                    ring.pop_front();
                }
                if was_paused || busy.load(Ordering::Relaxed) {
                    State::Idle
                } else {
                    stream.accept_waveform(RATE as i32, &f32s(&pcm));
                    let mut woke = None;
                    while k.is_ready(&stream) {
                        k.decode(&stream);
                        if let Some(r) = k.get_result(&stream).filter(|r| !r.keyword.is_empty()) {
                            woke = Some(r.keyword.clone());
                            k.reset(&stream);
                        }
                    }
                    match woke {
                        Some(w) => {
                            eprintln!("ears: wake ({w})");
                            if let Some(b) = burst.as_mut() {
                                b.2 = true;
                            }
                            sh_bg(&cfg.ears.wake_sound);
                            let mut c = out::Card::new("listening…");
                            c.lines.push(out::Fmt(&cfg.notify).color("accent", "say it"));
                            out::card(cfg, &c);
                            State::Hearing {
                                pcm: ring.iter().copied().collect(),
                                started: Instant::now(),
                                spoke: false,
                                quiet_ms: 0,
                            }
                        }
                        None => State::Idle,
                    }
                }
            }
            State::Hearing { pcm: mut rec, started, mut spoke, mut quiet_ms } => {
                rec.extend_from_slice(&pcm);
                let loud = rms(&pcm) > (floor * cfg.ears.speech_factor).max(cfg.voice.min_rms);
                if loud {
                    spoke = true;
                    quiet_ms = 0;
                } else {
                    quiet_ms += 100;
                }
                let age = started.elapsed();
                let done = (spoke && quiet_ms >= cfg.ears.silence_ms)
                    || age >= Duration::from_secs(cfg.ears.max_seconds)
                    // "MAGI, clip that" said in one breath: speech before the wake already counts,
                    // so a short pause after it also ends the request
                    || (!spoke && age >= Duration::from_millis(cfg.ears.start_timeout_ms));
                if !done {
                    State::Hearing { pcm: rec, started, spoke, quiet_ms }
                } else {
                    // a fresh stream: the old one still holds the wake word's state
                    stream = k.create_stream();
                    ring.clear();
                    busy.store(true, Ordering::Relaxed);
                    let (cfg2, h, b) = (cfg.clone(), handle.clone(), busy.clone());
                    std::thread::spawn(move || {
                        understand(&cfg2, &rec, &*h);
                        b.store(false, Ordering::Relaxed);
                    });
                    State::Idle
                }
            }
        };
        if let Ok(Some(st)) = child.try_wait() {
            return Err(format!("recorder exited ({st})"));
        }
    }
}

fn understand(cfg: &Config, pcm: &[i16], handle: &dyn Fn(&str)) {
    let t0 = Instant::now();
    let text = match llm::transcribe(&cfg.endpoint, &wav(pcm), &cfg.voice.language, &cfg.voice.prompt) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ears: speech failed: {e}");
            out::close_card(cfg);
            return;
        }
    };
    eprintln!("ears: heard {:?} ({} ms)", text.trim(), t0.elapsed().as_millis());
    let request = if cfg.ears.require_wake_word {
        match after_wake(cfg, &text) {
            Some(r) => r,
            None => {
                eprintln!("ears: no wake word in the transcript, ignored");
                out::close_card(cfg);
                return;
            }
        }
    } else {
        text.trim().to_string()
    };
    if request.trim().is_empty() {
        out::close_card(cfg);
        return;
    }
    handle(&request);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bpe_merges_best_pairs() {
        let p: HashMap<String, f32> =
            [("▁", -1.0), ("▁M", -2.0), ("A", -3.0), ("G", -3.0), ("I", -3.0), ("▁MA", -1.5), ("GI", -9.0)]
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect();
        assert_eq!(bpe_encode(&p, "magi").unwrap(), vec!["▁MA", "GI"]);
        assert!(bpe_encode(&p, "magix").is_none());
    }

    #[test]
    fn wav_roundtrip() {
        let pcm: Vec<i16> = (0..3200).map(|i| (i % 200) as i16 - 100).collect();
        assert_eq!(read_wav(&wav(&pcm)).unwrap(), pcm);
    }

    #[test]
    fn wake_word_in_transcript() {
        let c = config::parse(None).unwrap();
        assert_eq!(after_wake(&c, "Magi, clip that.").as_deref(), Some("clip that"));
        assert_eq!(after_wake(&c, "uh Maggie switch to workspace three").as_deref(), Some("switch to workspace 3"));
        assert_eq!(after_wake(&c, "we should push mid now"), None);
    }
}
