//! Claude (Anthropic Claude Code) harness implementation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Harness, HarnessKind, LaunchContext, RegisterContext, SessionKey};
use crate::tmux::Tmux;

pub struct ClaudeHarness;

impl Harness for ClaudeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    fn cli_binary(&self) -> &'static str {
        "claude"
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g @anthropic-ai/claude-code"
    }

    fn skill_hint(&self) -> &'static str {
        "Before starting, check if the repository has Claude Code skills defined in `.claude/skills/` or `.claude/commands/`. If any are relevant to your task, use them."
    }

    /// Build the launch command string. The system prompt is passed inline
    /// (`--system-prompt '<body>'`) on fresh launches; resumed sessions
    /// drop `--system-prompt` (the prompt is already baked into the saved
    /// thread).
    ///
    /// `--name <name>` registers the deterministic session name so the user
    /// can reattach manually with `claude --resume <name>` from a shell. It
    /// is passed on `SessionKey::Auto` and `SessionKey::Pin` only — on
    /// `Resume` the session already has a stored name, and re-passing
    /// `--name` would risk overwriting it. On `SessionKey::Pin` the session
    /// is also pinned to a known UUID via `--session-id <uuid>` so a later
    /// `--resume <uuid>` lands directly in interactive mode (resuming by
    /// name opens a picker).
    fn build_session_command(&self, ctx: &LaunchContext) -> String {
        let mut cmd = String::from("claude --dangerously-skip-permissions");

        match ctx.session_key {
            SessionKey::Auto => {
                let escaped_name = ctx.name.replace('\'', "'\\''");
                let escaped_prompt = ctx.identity.replace('\'', "'\\''");
                cmd.push_str(&format!(" --name '{}'", escaped_name));
                cmd.push_str(&format!(" --system-prompt '{}'", escaped_prompt));
            }
            SessionKey::Pin(uuid) => {
                let escaped_name = ctx.name.replace('\'', "'\\''");
                let escaped_uuid = uuid.replace('\'', "'\\''");
                let escaped_prompt = ctx.identity.replace('\'', "'\\''");
                cmd.push_str(&format!(" --name '{}'", escaped_name));
                cmd.push_str(&format!(" --session-id '{}'", escaped_uuid));
                cmd.push_str(&format!(" --system-prompt '{}'", escaped_prompt));
            }
            SessionKey::Resume(uuid) => {
                let escaped_uuid = uuid.replace('\'', "'\\''");
                // Resumed sessions keep their original prompt and stored
                // name; do NOT pass --system-prompt or --name (the former
                // would be ignored or replace the saved prompt; the latter
                // would risk rewriting the display name and silently drift
                // if the deterministic name ever changed).
                cmd.push_str(&format!(" --resume '{}'", escaped_uuid));
            }
        }
        cmd
    }

    fn register_session_name(&self, _ctx: &RegisterContext) -> Result<()> {
        // Claude registers the name via `--name <name>` at launch time. No
        // post-launch step needed.
        Ok(())
    }

    /// Pre-stamp workspace trust in `~/.claude.json` so the interactive
    /// "trust this folder?" dialog does not block first launch in `cwd`.
    ///
    /// `~/.claude.json` is a root-level dot file (NOT inside `~/.claude/`).
    /// Tests can bypass `dirs::home_dir()` via the
    /// `harness::ensure_workspace_trusted_for_test` explicit-path seam.
    fn ensure_workspace_trusted(&self, cwd: &Path) -> Result<()> {
        ensure_workspace_trusted_in(&claude_trust_file_path(), cwd)
    }

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        kill_pane_via_slash(session, window, "/exit", 2)
    }
}

/// Kill ladder shared by both harnesses: send a slash command, wait for the
/// pane to return to a shell prompt, then escalate to Ctrl-C × N as a
/// fallback. Used internally by `Harness::kill_pane` impls.
pub(crate) fn kill_pane_via_slash(
    session: &str,
    window: Option<&str>,
    slash: &str,
    ctrl_c_count: usize,
) -> Result<()> {
    // Bail early if the pane isn't even queryable, or already at a shell.
    let still_running = match Tmux::is_agent_running_in(session, window) {
        Ok((running, _)) => running,
        Err(e) => {
            tracing::debug!(
                session = session,
                error = %e,
                "kill_pane: pane not queryable; nothing to kill"
            );
            return Ok(());
        }
    };
    if !still_running {
        tracing::debug!(session = session, "kill_pane: pane already at shell; no-op");
        return Ok(());
    }

    let target_window = window.unwrap_or("");
    tracing::debug!(session = session, slash, "kill_pane: sending slash command");
    let send_result = match window {
        Some(w) => Tmux::send_keys_to_window(session, w, slash),
        None => Tmux::send_keys_to_session(session, slash),
    };
    match send_result {
        Ok(()) => {
            if wait_for_pane_idle(session, window, Duration::from_secs(5)) {
                return Ok(());
            }
            tracing::warn!(
                session = session,
                slash,
                "slash command did not return pane to shell within 5s; escalating to Ctrl-C"
            );
        }
        Err(e) => {
            tracing::warn!(
                session = session,
                slash,
                error = %e,
                "failed to send slash command; escalating to Ctrl-C"
            );
        }
    }

    for i in 0..ctrl_c_count {
        // Before the 2nd through Nth Ctrl-C, give the agent a brief window
        // to actually quit from the previous press. If the pane is already
        // idle we'd otherwise be over-pressing into the freshly-revealed
        // shell.
        if i > 0 && wait_for_pane_idle(session, window, Duration::from_millis(150)) {
            return Ok(());
        }
        let _ = match window {
            Some(w) => Tmux::send_ctrl_c_to_window(session, w),
            None => Tmux::send_ctrl_c_to_session(session),
        };
        std::thread::sleep(Duration::from_millis(150));
    }

    if wait_for_pane_idle(session, window, Duration::from_secs(2)) {
        return Ok(());
    }

    anyhow::bail!(
        "failed to quit agent in pane '{}:{}' after {} + Ctrl-C x{}",
        session,
        target_window,
        slash,
        ctrl_c_count
    )
}

pub(crate) fn wait_for_pane_idle(session: &str, window: Option<&str>, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok((false, _)) = Tmux::is_agent_running_in(session, window) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// Resolve the path to claude's per-user trust/state file (`~/.claude.json`,
/// a root-level dot file — NOT inside `~/.claude/`). Tests bypass this via
/// the explicit-path `ensure_workspace_trusted_in` overload.
fn claude_trust_file_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".claude.json")
}

/// Ensure `projects["<cwd>"].hasTrustDialogAccepted == true` is present in
/// the claude per-user JSON file at `trust_file` (typically
/// `~/.claude.json`). Tests pass an explicit `TempDir`-backed path.
///
/// Behavior:
/// - File doesn't exist → create one with just the trust entry.
/// - File exists but has no `projects` key → add it.
/// - `projects[<cwd>]` exists but is missing `hasTrustDialogAccepted` (or
///   it's `false`) → set to `true`, leave all other sub-keys
///   (`allowedTools`, `mcpServers`, etc.) untouched.
/// - `projects[<cwd>].hasTrustDialogAccepted` already `true` → no-op
///   (file untouched, mtime unchanged).
///
/// All other keys (root-level + other project entries) are preserved
/// byte-for-byte across the read/write round-trip — `serde_json` re-emits
/// the value tree, but only entries we don't touch ride through unchanged
/// (within JSON's value-equivalence — formatting may differ).
pub fn ensure_workspace_trusted_in(trust_file: &Path, cwd: &Path) -> Result<()> {
    use anyhow::Context;

    let cwd_str = cwd.to_string_lossy().to_string();

    // Parse existing JSON or start from `{}`.
    let mut root: serde_json::Value = if trust_file.exists() {
        let text = std::fs::read_to_string(trust_file)
            .with_context(|| format!("read claude trust file at {}", trust_file.display()))?;
        if text.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&text).with_context(|| {
                format!(
                    "parse claude trust file at {} as JSON",
                    trust_file.display()
                )
            })?
        }
    } else {
        serde_json::json!({})
    };

    let obj = root.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "claude trust file at {} is not a JSON object",
            trust_file.display()
        )
    })?;

    // Idempotent fast-path: peek before mutating so we can skip the write
    // when the entry is already trusted (preserves mtime).
    let already_trusted = obj
        .get("projects")
        .and_then(|p| p.as_object())
        .and_then(|p| p.get(&cwd_str))
        .and_then(|v| v.as_object())
        .and_then(|t| t.get("hasTrustDialogAccepted"))
        .and_then(|v| v.as_bool())
        == Some(true);
    if already_trusted {
        return Ok(());
    }

    // Walk into projects.<cwd>.hasTrustDialogAccepted and set it.
    if !obj.contains_key("projects") {
        obj.insert("projects".to_string(), serde_json::json!({}));
    }
    let projects = obj
        .get_mut("projects")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "claude trust file at {} has non-object `projects`",
                trust_file.display()
            )
        })?;

    if !projects.contains_key(&cwd_str) {
        projects.insert(cwd_str.clone(), serde_json::json!({}));
    }
    let project = projects
        .get_mut(&cwd_str)
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "claude trust file at {} has non-object `projects[\"{}\"]`",
                trust_file.display(),
                cwd_str
            )
        })?;

    project.insert(
        "hasTrustDialogAccepted".to_string(),
        serde_json::Value::Bool(true),
    );

    let serialized = serde_json::to_vec_pretty(&root)?;
    write_atomically(trust_file, &serialized)
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
