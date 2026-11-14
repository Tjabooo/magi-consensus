//! Running commands: context snapshots and the approved action.

use crate::config::{self, Run};
use serde::Serialize;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// None = killed after the timeout
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub ms: u128,
    pub timed_out: bool,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

fn path_env(run: &Run) -> String {
    let mut parts: Vec<String> = run.path_prepend.iter().map(|p| config::expand(p).display().to_string()).collect();
    if let Ok(p) = std::env::var("PATH") {
        parts.push(p);
    }
    parts.join(":")
}

fn reader(mut r: impl Read + Send + 'static, cap: usize) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let b = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut g = b.lock().unwrap();
            let room = cap.saturating_sub(g.len());
            g.extend_from_slice(&chunk[..n.min(room)]);
        }
    });
    buf
}

/// Run `command` through `run.shell` in its own process group; kill the group after `timeout`.
/// Output readers aren't waited for after exit (an app the command launched may keep the pipe open).
pub fn run(run: &Run, command: &str, timeout: Duration) -> Outcome {
    let t = Instant::now();
    let mut cmd = Command::new(&run.shell[0]);
    cmd.args(&run.shell[1..]).arg(command)
        .current_dir(config::expand(&run.cwd))
        .env("PATH", path_env(run))
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome { code: Some(127), stdout: String::new(), stderr: format!("can't start {}: {e}", run.shell[0]), ms: 0, timed_out: false },
    };
    let out = reader(child.stdout.take().unwrap(), run.max_output);
    let err = reader(child.stderr.take().unwrap(), run.max_output);
    let deadline = t + timeout;
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())])
                    .stderr(Stdio::null()).status();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => break None,
        }
    };
    std::thread::sleep(Duration::from_millis(30)); // let the readers drain what's already written
    let text = |b: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock().unwrap()).trim().to_string();
    Outcome { code, stdout: text(&out), stderr: text(&err), ms: t.elapsed().as_millis(), timed_out }
}

/// `$ cmd` + output for each context command, capped at `max_chars` in total.
pub fn context(cfg: &config::Config) -> String {
    let mut s = String::new();
    for c in &cfg.context.commands {
        let o = run(&cfg.run, c, Duration::from_secs(3));
        let body = if o.stdout.is_empty() { o.stderr } else { o.stdout };
        s.push_str(&format!("$ {c}\n{body}\n"));
    }
    if s.chars().count() > cfg.context.max_chars {
        s = s.chars().take(cfg.context.max_chars).collect::<String>() + "…";
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Run {
        config::parse(None).unwrap().run
    }

    #[test]
    fn captures_output_and_code() {
        let o = run(&r(), "echo hi; echo err >&2; exit 3", Duration::from_secs(5));
        assert_eq!(o.code, Some(3));
        assert_eq!(o.stdout, "hi");
        assert_eq!(o.stderr, "err");
    }

    #[test]
    fn kills_after_timeout() {
        let o = run(&r(), "sleep 5", Duration::from_millis(200));
        assert!(o.timed_out);
        assert!(o.ms < 2000);
    }
}
