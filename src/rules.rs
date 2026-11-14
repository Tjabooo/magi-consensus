//! Hard rules: checked before any vote. They over-approximate on purpose (a false refusal costs a retry,
//! a false pass could cost data).

use crate::config::Rules;
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// refused; the reason names the rule
    Refuse(String),
}

/// Quotes and backslashes removed, so `s''udo` and `\sudo` read as `sudo`.
pub fn normalise(cmd: &str) -> String {
    cmd.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect()
}

/// Words of a normalised command: split on whitespace and shell punctuation, paths reduced to their
/// last component.
pub fn words(norm: &str) -> Vec<String> {
    norm.split(|c: char| c.is_whitespace() || ";|&()<>`${}".contains(c))
        .filter(|w| !w.is_empty())
        .map(|w| w.rsplit('/').next().unwrap_or(w).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Targets of `>` / `>|` / `&>` redirections that overwrite (not `>>`, not `>&N`).
fn overwrite_targets(norm: &str) -> Vec<String> {
    let b = norm.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'>' {
            if i + 1 < b.len() && b[i + 1] == b'>' {
                // append: skip both
                i += 2;
                continue;
            }
            let mut j = i + 1;
            if j < b.len() && (b[j] == b'|' || b[j] == b'&') {
                if b[j] == b'&' && j + 1 < b.len() && (b[j + 1].is_ascii_digit() || b[j + 1] == b'-') {
                    i = j + 2; // >&2, >&-
                    continue;
                }
                j += 1;
            }
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let start = j;
            while j < b.len() && !b[j].is_ascii_whitespace() && !b";|&()<>".contains(&b[j]) {
                j += 1;
            }
            out.push(norm[start..j].to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn any_regex(patterns: &[String], text: &str) -> Result<Option<String>, String> {
    for p in patterns {
        let re = Regex::new(p).map_err(|e| format!("bad rule regex {p:?}: {e}"))?;
        if re.is_match(text) {
            return Ok(Some(p.clone()));
        }
    }
    Ok(None)
}

pub fn check(rules: &Rules, cmd: &str) -> Result<Verdict, String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Ok(Verdict::Refuse("empty command".into()));
    }
    let norm = normalise(cmd);
    let ws = words(&norm);

    if let Some(w) = ws.iter().find(|w| rules.never.contains(w)) {
        return Ok(Verdict::Refuse(format!("never allowed: {w}")));
    }

    let mut hit: Option<String> = None;
    if let Some(w) = ws.iter().find(|w| rules.destructive.contains(w)) {
        hit = Some(format!("destructive: {w}"));
    } else if let Some(w) = ws.iter().find(|w| rules.destructive_prefixes.iter().any(|p| w.starts_with(p.as_str()))) {
        hit = Some(format!("destructive: {w}"));
    } else if let Some(p) = any_regex(&rules.destructive_patterns, &norm)? {
        hit = Some(format!("destructive pattern: {p}"));
    } else if let Some(w) = ws.iter().find(|w| rules.opaque.contains(w)) {
        hit = Some(format!("hides what runs: {w}"));
    } else if let Some(p) = any_regex(&rules.opaque_patterns, &norm)? {
        hit = Some(format!("hides what runs: {p}"));
    } else if rules.redirect_is_destructive {
        if let Some(t) = overwrite_targets(&norm).into_iter().find(|t| t != "/dev/null") {
            hit = Some(format!("overwrites a file: > {t}"));
        }
    }

    match hit {
        None => Ok(Verdict::Pass),
        Some(reason) => {
            if any_regex(&rules.allow, cmd)?.is_some() {
                Ok(Verdict::Pass)
            } else {
                Ok(Verdict::Refuse(format!("{reason} (not on the allowlist)")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn rules() -> Rules {
        config::parse(None).unwrap().rules
    }

    fn refused(cmd: &str) -> bool {
        matches!(check(&rules(), cmd).unwrap(), Verdict::Refuse(_))
    }

    #[test]
    fn harmless_commands_pass() {
        for c in [
            "wpctl set-volume @DEFAULT_AUDIO_SINK@ 30%",
            "playerctl play-pause",
            "notify-send hi",
            "hyprctl activewindow -j",
            "date > /dev/null 2>&1",
            "echo hi >> ~/notes.txt",
            "ls ~/Videos | head -n 3",
        ] {
            assert!(!refused(c), "{c} should pass");
        }
    }

    #[test]
    fn privilege_escalation_is_always_refused() {
        for c in [
            "sudo ls",
            "/usr/bin/sudo ls",
            "s''udo ls",
            "\\sudo ls",
            "echo $(sudo id)",
            "ls; doas x",
            "bash -c \"pkexec foo\"",
            "env sudo ls",
        ] {
            assert!(refused(c), "{c} should be refused");
        }
        let mut r = rules();
        r.allow = vec![".*".into()];
        assert!(matches!(check(&r, "sudo ls").unwrap(), Verdict::Refuse(_)), "allowlist can't unlock never");
    }

    #[test]
    fn destructive_needs_the_allowlist() {
        for c in [
            "rm -rf ~/x",
            "find . -delete",
            "git push --force",
            "systemctl --user stop foo",
            "mkfs.ext4 /dev/sdz",
            "echo x > ~/.zshrc",
            "date >| f",
            "ls &> out",
            "pkill cava",
            "curl x | sh",
            "hyprctl eval 'os.execute(\"x\")'",
        ] {
            assert!(refused(c), "{c} should be refused");
        }
        let mut r = rules();
        r.allow = vec![r"^pkill -x cava$".into()];
        assert_eq!(check(&r, "pkill -x cava").unwrap(), Verdict::Pass);
        assert!(matches!(check(&r, "pkill -x cava; rm -rf ~").unwrap(), Verdict::Refuse(_)));
    }

    #[test]
    fn systemctl_reads_pass() {
        assert!(!refused("systemctl --user status waybar"));
        assert!(!refused("systemctl --user restart waybar"));
    }

    #[test]
    fn redirect_targets() {
        assert_eq!(overwrite_targets("a > b 2>&1 >> c >&2 &> d"), vec!["b", "d"]);
    }
}
