//! Codex (OpenAI Codex CLI) harness implementation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Harness, HarnessKind, LaunchContext, RegisterContext, SessionKey};

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
        // Resume short-circuits: `codex resume <name>` keeps the saved
        // thread's developer_instructions, so we skip the `-c ...` arg.
        // Pass the working directory via `-C <cwd>` so codex doesn't
        // prompt a directory picker when launch cwd differs from saved.
        if let SessionKey::Resume(name) = ctx.session_key {
            let cwd_str = ctx.cwd.to_string_lossy().replace('\'', "'\\''");
            let escaped_name = name.replace('\'', "'\\''");
            let mut cmd = String::from("codex");
            // Always run codex with full approval+sandbox bypass. Mirrors
            // claude's `--dangerously-skip-permissions`. Without this, codex
            // prompts before privileged-feeling shell commands (`git add`,
            // etc.), which deadlocks autonomous agman flows.
            cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
            if ctx.no_alt_screen {
                cmd.push_str(" --no-alt-screen");
            }
            cmd.push_str(&format!(" -C '{}'", cwd_str));
            cmd.push_str(&format!(" resume '{}'", escaped_name));
            return cmd;
        }

        // Auto and Pin: identical fresh-launch shape. Codex doesn't accept
        // a launch-time session-id pin; the deterministic name is
        // registered post-launch via `/rename`.
        // Codex consumes identity via TOML `developer_instructions`. Use
        // triple-quoted strings so newlines are preserved verbatim. Defensive
        // escape literal `"""` in the body.
        let body = ctx.identity.replace("\"\"\"", "\\\"\\\"\\\"");
        let dev_instructions = format!("developer_instructions=\"\"\"{}\"\"\"", body);
        // Single-quote the whole `-c` arg, escaping inner single quotes.
        let dev_arg_escaped = dev_instructions.replace('\'', "'\\''");

        let mut cmd = String::from("codex");
        // Always run codex with full approval+sandbox bypass. Mirrors
        // claude's `--dangerously-skip-permissions`. Without this, codex
        // prompts before privileged-feeling shell commands (`git add`, etc.),
        // which deadlocks autonomous agman flows.
        cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
        if ctx.no_alt_screen {
            cmd.push_str(" --no-alt-screen");
        }
        cmd.push_str(&format!(" -c '{}'", dev_arg_escaped));
        cmd
    }

    /// Pre-stamp workspace trust in `~/.codex/config.toml` so the
    /// interactive trust dialog does not block first launch in `cwd`.
    fn ensure_workspace_trusted(&self, cwd: &Path) -> Result<()> {
        let trust_file = super::harness_home(HarnessKind::Codex).join("config.toml");
        ensure_workspace_trusted_in(&trust_file, cwd)
    }

    /// Paste-inject `/rename <name>` post-launch and verify the entry shows
    /// up in `~/.codex/session_index.jsonl`. Self-verifying with retry: codex
    /// step 2+ relaunches faster than first launch (file watchers warm, no
    /// first-time prompts), so the bracket-paste handler isn't always
    /// fully mounted when `wait_for_agent_ready` returns. Sleep ~500 ms,
    /// then loop up to 3 attempts of `paste + 2 s poll`. On all-three timeout,
    /// log a warning and return Ok — the session is still usable, just not
    /// resume-by-name.
    fn register_session_name(&self, ctx: &RegisterContext) -> Result<()> {
        let target = match ctx.window {
            Some(w) => format!("{}:{}", ctx.session, w),
            None => ctx.session.to_string(),
        };
        let cmd = format!("/rename {}", ctx.name);
        let index_path = ctx.harness_home.join("session_index.jsonl");

        let found = register_session_name_with_retry(
            || paste_text(&target, &cmd),
            &index_path,
            ctx.name,
            Duration::from_millis(500),
            Duration::from_secs(2),
            3,
        )?;

        if found {
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
                "codex /rename did not appear in session_index.jsonl after 3 retries; session usable but not resume-by-name"
            );
        }
        Ok(())
    }

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        super::claude::kill_pane_via_slash(session, window, "/quit", 3)
    }
}

/// Run `paste_attempt` then poll `index_path` for `name`, retrying up to
/// `max_attempts` times. Returns `Ok(true)` if the entry appears within any
/// attempt's poll window, `Ok(false)` if all attempts time out.
///
/// `initial_delay` is slept before the first paste attempt — codex's
/// bracket-paste input handler is not always mounted at the moment
/// `wait_for_agent_ready` returns true (especially on step 2+ relaunches
/// where everything is hot). The delay gives the TUI time to wire it up.
///
/// Per-attempt: paste failures are logged at warn but do NOT short-circuit
/// the loop — the next attempt may succeed (e.g., transient tmux race).
/// Inter-attempt: no extra sleep beyond the polling timeout, which
/// effectively backs off naturally.
pub(crate) fn register_session_name_with_retry<F>(
    mut paste_attempt: F,
    index_path: &Path,
    name: &str,
    initial_delay: Duration,
    poll_timeout: Duration,
    max_attempts: u32,
) -> anyhow::Result<bool>
where
    F: FnMut() -> anyhow::Result<()>,
{
    if !initial_delay.is_zero() {
        std::thread::sleep(initial_delay);
    }
    for attempt in 1..=max_attempts {
        if let Err(e) = paste_attempt() {
            tracing::warn!(
                attempt,
                name = name,
                error = %e,
                "codex /rename: paste attempt failed; will retry"
            );
        }
        if poll_session_index_for(index_path, name, poll_timeout) {
            return Ok(true);
        }
    }
    Ok(false)
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
/// Handles both flat and nested shapes by walking JSON values for any
/// `thread_name` (codex's actual key) or `name` field whose value matches.
fn line_names_match(line: &str, name: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    json_contains_name(&v, name)
}

fn json_contains_name(v: &serde_json::Value, name: &str) -> bool {
    match v {
        serde_json::Value::Object(map) => {
            // Codex writes `thread_name` in session_index.jsonl. Accept
            // `name` too as a forward-compat fallback.
            for key in ["thread_name", "name"] {
                if let Some(serde_json::Value::String(s)) = map.get(key) {
                    if s == name {
                        return true;
                    }
                }
            }
            map.values().any(|child| json_contains_name(child, name))
        }
        serde_json::Value::Array(arr) => arr.iter().any(|child| json_contains_name(child, name)),
        _ => false,
    }
}

/// Check whether `~/.codex/session_index.jsonl` already contains a thread
/// named `name`. Used to decide between a fresh codex launch and `codex
/// resume <name>` for long-lived agents. Reuses the same walker as the
/// post-`/rename` poll, so a fix to one benefits both paths.
pub fn codex_has_session(harness_home: &Path, name: &str) -> bool {
    let index_path = harness_home.join("session_index.jsonl");
    let Ok(content) = std::fs::read_to_string(&index_path) else {
        return false;
    };
    content.lines().any(|line| line_names_match(line, name))
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

/// Ensure `[projects."<cwd>"] trust_level = "trusted"` is present in the
/// codex config TOML at `trust_file` (typically `~/.codex/config.toml`).
/// Tests pass an explicit `TempDir`-backed path; production calls into
/// `harness_home(Codex).join("config.toml")` which honors `AGMAN_CODEX_HOME`.
///
/// Behavior:
/// - File doesn't exist → create one with just the trust entry.
/// - File exists, no `[projects."<cwd>"]` table → add it.
/// - Table exists but no `trust_level` → set it to `"trusted"`.
/// - `trust_level` already `"trusted"` → no-op (file untouched).
/// - `trust_level` is `"untrusted"` → upgrade to `"trusted"`.
///
/// Other keys (root-level + other project tables) are preserved. Layout /
/// comments may be rewritten by `toml::to_string` — acceptable here because
/// the codex config is mostly machine-generated.
pub fn ensure_workspace_trusted_in(trust_file: &Path, cwd: &Path) -> Result<()> {
    use anyhow::Context;

    let cwd_str = cwd.to_string_lossy().to_string();

    // Parse the existing TOML or start from an empty table.
    let mut doc: toml::Value = if trust_file.exists() {
        let text = std::fs::read_to_string(trust_file)
            .with_context(|| format!("read codex trust file at {}", trust_file.display()))?;
        if text.trim().is_empty() {
            toml::Value::Table(toml::value::Table::new())
        } else {
            toml::from_str(&text).with_context(|| {
                format!("parse codex trust file at {} as TOML", trust_file.display())
            })?
        }
    } else {
        toml::Value::Table(toml::value::Table::new())
    };

    let root = doc.as_table_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "codex trust file at {} is not a TOML table",
            trust_file.display()
        )
    })?;

    // Idempotent fast-path: peek before mutating so we can skip the write
    // when the entry is already trusted (preserves mtime).
    let already_trusted = root
        .get("projects")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get(&cwd_str))
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("trust_level"))
        .and_then(|v| v.as_str())
        == Some("trusted");
    if already_trusted {
        return Ok(());
    }

    // Walk into projects.<cwd>.trust_level and set it.
    if !root.contains_key("projects") {
        root.insert(
            "projects".to_string(),
            toml::Value::Table(toml::value::Table::new()),
        );
    }
    let projects = root
        .get_mut("projects")
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "codex trust file at {} has non-table `projects`",
                trust_file.display()
            )
        })?;

    if !projects.contains_key(&cwd_str) {
        projects.insert(
            cwd_str.clone(),
            toml::Value::Table(toml::value::Table::new()),
        );
    }
    let project = projects
        .get_mut(&cwd_str)
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "codex trust file at {} has non-table `projects.\"{}\"`",
                trust_file.display(),
                cwd_str
            )
        })?;

    project.insert(
        "trust_level".to_string(),
        toml::Value::String("trusted".to_string()),
    );

    write_atomically(trust_file, toml::to_string(&doc)?.as_bytes())
}

/// Write `bytes` to `dest` atomically: write to `<dest>.tmp`, fsync, rename.
/// Creates `dest`'s parent directory if missing.
fn write_atomically(dest: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_path = dest.as_os_str().to_owned();
    tmp_path.push(".tmp");
    let tmp_path = PathBuf::from(tmp_path);
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, dest)?;
    Ok(())
}
