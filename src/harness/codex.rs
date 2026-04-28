//! Codex (OpenAI Codex CLI) harness implementation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Harness, HarnessKind, LaunchContext, RegisterContext};

pub struct CodexHarness;

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn cli_binary(&self) -> &'static str {
        "codex"
    }

    fn install_hint(&self) -> &'static str {
        "brew install --cask codex (macOS) / npm install -g @openai/codex"
    }

    fn skill_hint(&self) -> &'static str {
        // Per locked decision: skip the skill hint for codex.
        ""
    }

    fn build_session_command(&self, ctx: &LaunchContext) -> String {
        // Codex consumes identity via TOML `developer_instructions`. Use
        // triple-quoted strings so newlines are preserved verbatim. Defensive
        // escape literal `"""` in the body.
        let body = ctx.identity.replace("\"\"\"", "\\\"\\\"\\\"");
        let dev_instructions = format!(
            "developer_instructions=\"\"\"{}\"\"\"",
            body
        );
        // Single-quote the whole `-c` arg, escaping inner single quotes.
        let dev_arg_escaped = dev_instructions.replace('\'', "'\\''");

        let mut cmd = String::from("codex");
        if ctx.skip_git_repo_check {
            cmd.push_str(" --skip-git-repo-check");
        }
        if ctx.no_alt_screen {
            cmd.push_str(" --no-alt-screen");
        }
        cmd.push_str(&format!(" -c '{}'", dev_arg_escaped));
        cmd
    }

    /// Paste-inject `/rename <name>` post-launch and verify the entry shows
    /// up in `~/.codex/session_index.jsonl` within 5s. On timeout, log a
    /// warning and return Ok — the session is still usable, just not
    /// resume-by-name.
    fn register_session_name(&self, ctx: &RegisterContext) -> Result<()> {
        let target = match ctx.window {
            Some(w) => format!("{}:{}", ctx.session, w),
            None => ctx.session.to_string(),
        };
        let cmd = format!("/rename {}", ctx.name);

        // Use load-buffer + paste-buffer + Enter to inject the slash command
        // without depending on shell escaping.
        if let Err(e) = paste_text(&target, &cmd) {
            tracing::warn!(
                session = ctx.session,
                name = ctx.name,
                error = %e,
                "codex register_session_name: failed to paste /rename; continuing"
            );
            return Ok(());
        }

        // Tail session_index.jsonl for up to 5s, looking for an entry whose
        // name matches.
        let index_path = ctx.harness_home.join("session_index.jsonl");
        if poll_session_index_for(&index_path, ctx.name, Duration::from_secs(5)) {
            tracing::debug!(
                session = ctx.session,
                name = ctx.name,
                "codex /rename registered in session_index.jsonl"
            );
        } else {
            tracing::warn!(
                session = ctx.session,
                name = ctx.name,
                index_path = %index_path.display(),
                "codex /rename did not appear in session_index.jsonl within 5s; session usable but not resume-by-name"
            );
        }
        Ok(())
    }

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        super::claude::kill_pane_via_slash(session, window, "/quit", 3)
    }

    /// Walk `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (last 7 days only,
    /// for performance) and return the most-recently-modified file whose
    /// first line declares a matching `cwd`.
    fn latest_transcript(&self, cwd: &Path) -> Option<PathBuf> {
        let sessions_root = super::harness_home(HarnessKind::Codex).join("sessions");
        if !sessions_root.exists() {
            return None;
        }
        let cwd_canonical = std::fs::canonicalize(cwd)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| cwd.to_string_lossy().to_string());

        // Collect candidate rollout files from at most the last 7 day-buckets.
        let mut candidates: Vec<PathBuf> = Vec::new();
        for year_entry in std::fs::read_dir(&sessions_root).ok()?.flatten() {
            let year_path = year_entry.path();
            if !year_path.is_dir() {
                continue;
            }
            for month_entry in std::fs::read_dir(&year_path).ok().into_iter().flatten().flatten() {
                let month_path = month_entry.path();
                if !month_path.is_dir() {
                    continue;
                }
                for day_entry in std::fs::read_dir(&month_path).ok().into_iter().flatten().flatten() {
                    let day_path = day_entry.path();
                    if !day_path.is_dir() {
                        continue;
                    }
                    for file_entry in std::fs::read_dir(&day_path).ok().into_iter().flatten().flatten() {
                        let p = file_entry.path();
                        if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            candidates.push(p);
                        }
                    }
                }
            }
        }

        // Sort newest-first by mtime and short-circuit on cwd match.
        candidates.sort_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
        candidates.reverse();

        // Cap to a reasonable horizon to avoid scanning huge histories.
        let now = std::time::SystemTime::now();
        let max_age = Duration::from_secs(7 * 24 * 60 * 60);

        for path in candidates {
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(age) = now.duration_since(mtime) {
                        if age > max_age {
                            continue;
                        }
                    }
                }
            }
            if rollout_cwd_matches(&path, &cwd_canonical, cwd) {
                return Some(path);
            }
        }
        None
    }

    /// Last `event_msg` with `type=agent_message` — return its `timestamp`
    /// (or hash-of-message+ts if no timestamp). Stable per assistant turn.
    fn find_last_assistant_marker(&self, transcript: &Path) -> Option<String> {
        let content = std::fs::read_to_string(transcript).ok()?;
        let mut last: Option<String> = None;
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            // Codex rollout entries: { "type": "event_msg", "payload": { "type": "agent_message", ... }, "timestamp": ... }
            // Be permissive about field placement: walk both top-level and payload.
            let outer_type = v.get("type").and_then(|t| t.as_str());
            let payload_type = v
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());

            let is_agent_message = payload_type == Some("agent_message")
                || (outer_type == Some("agent_message"));
            if !is_agent_message {
                continue;
            }
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .or_else(|| {
                    v.get("payload")
                        .and_then(|p| p.get("timestamp"))
                        .and_then(|t| t.as_str())
                });
            if let Some(t) = ts {
                last = Some(t.to_string());
            } else {
                // Fallback: stable-ish hash of the line itself.
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::{Hash, Hasher};
                line.hash(&mut hasher);
                last = Some(format!("h{:x}", hasher.finish()));
            }
        }
        last
    }
}

/// Read the first line of a rollout file and decide whether its declared
/// `cwd` matches `cwd_canonical` (or, as a fallback, the raw `cwd` we were
/// asked about).
fn rollout_cwd_matches(path: &Path, cwd_canonical: &str, cwd_raw: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(first) = content.lines().next() else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(first) else {
        return false;
    };
    // Be permissive about exactly where `cwd` lives in the schema. Codex's
    // current rollouts store it under `payload.cwd` for `session_meta`
    // entries; older / newer versions may differ.
    let candidates = [
        v.get("cwd").and_then(|c| c.as_str()),
        v.get("payload").and_then(|p| p.get("cwd")).and_then(|c| c.as_str()),
        v.get("payload")
            .and_then(|p| p.get("session_meta"))
            .and_then(|s| s.get("cwd"))
            .and_then(|c| c.as_str()),
    ];
    let raw_str = cwd_raw.to_string_lossy();
    for c in candidates.iter().flatten() {
        if *c == cwd_canonical || *c == raw_str.as_ref() {
            return true;
        }
    }
    false
}

/// Poll `index_path` (`~/.codex/session_index.jsonl`) for at most `timeout`
/// looking for any line containing an entry named `name`. Returns true if
/// such a line is observed; false on timeout.
pub(crate) fn poll_session_index_for(index_path: &Path, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut last_size: u64 = 0;
    while Instant::now() < deadline {
        let size = std::fs::metadata(index_path).map(|m| m.len()).unwrap_or(0);
        if size != last_size {
            if let Ok(content) = std::fs::read_to_string(index_path) {
                for line in content.lines() {
                    if line_names_match(line, name) {
                        return true;
                    }
                }
            }
            last_size = size;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Return true when a session_index.jsonl line declares the given `name`.
/// Handles both flat (`{"name": "..."}`) and nested (`{"session": {"name":
/// "..."}}`) shapes by walking JSON values for any `name` field whose value
/// matches.
fn line_names_match(line: &str, name: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    json_contains_name(&v, name)
}

fn json_contains_name(v: &serde_json::Value, name: &str) -> bool {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get("name") {
                if s == name {
                    return true;
                }
            }
            map.values().any(|child| json_contains_name(child, name))
        }
        serde_json::Value::Array(arr) => arr.iter().any(|child| json_contains_name(child, name)),
        _ => false,
    }
}

/// Paste `text` into a tmux target as a single block followed by Enter,
/// using load-buffer + paste-buffer (bracket paste mode) so newlines and
/// shell metacharacters survive.
fn paste_text(target: &str, text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("tmux")
        .args(["load-buffer", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(text.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux load-buffer failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let paste = Command::new("tmux")
        .args(["paste-buffer", "-p", "-t", target])
        .output()?;
    if !paste.status.success() {
        anyhow::bail!(
            "tmux paste-buffer failed: {}",
            String::from_utf8_lossy(&paste.stderr)
        );
    }

    std::thread::sleep(Duration::from_millis(200));
    let enter = Command::new("tmux")
        .args(["send-keys", "-t", target, "Enter"])
        .output()?;
    if !enter.status.success() {
        anyhow::bail!(
            "tmux send-keys (Enter) failed: {}",
            String::from_utf8_lossy(&enter.stderr)
        );
    }
    Ok(())
}
