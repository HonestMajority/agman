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
    /// can reattach manually with `claude --resume <name>` from a shell.
    /// On `SessionKey::Pin` the session is also pinned to a known UUID via
    /// `--session-id <uuid>` so a later `--resume <uuid>` lands directly in
    /// interactive mode (resuming by name opens a picker).
    fn build_session_command(&self, ctx: &LaunchContext) -> String {
        let escaped_name = ctx.name.replace('\'', "'\\''");

        let mut cmd = String::from("claude --dangerously-skip-permissions");
        cmd.push_str(&format!(" --name '{}'", escaped_name));

        match ctx.session_key {
            SessionKey::Auto => {
                let escaped_prompt = ctx.identity.replace('\'', "'\\''");
                cmd.push_str(&format!(" --system-prompt '{}'", escaped_prompt));
            }
            SessionKey::Pin(uuid) => {
                let escaped_uuid = uuid.replace('\'', "'\\''");
                let escaped_prompt = ctx.identity.replace('\'', "'\\''");
                cmd.push_str(&format!(" --session-id '{}'", escaped_uuid));
                cmd.push_str(&format!(" --system-prompt '{}'", escaped_prompt));
            }
            SessionKey::Resume(uuid) => {
                let escaped_uuid = uuid.replace('\'', "'\\''");
                // Resumed sessions keep their original prompt; do NOT pass
                // --system-prompt (it would be ignored or worse, replace
                // the saved one).
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

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        kill_pane_via_slash(session, window, "/exit", 2)
    }

    fn latest_transcript(&self, cwd: &Path) -> Option<PathBuf> {
        latest_transcript_in(&super::harness_home(HarnessKind::Claude), cwd)
    }

    fn find_last_assistant_marker(&self, transcript: &Path) -> Option<String> {
        let content = std::fs::read_to_string(transcript).ok()?;
        let mut last_uuid: Option<String> = None;
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                    if let Some(uuid) = v.get("uuid").and_then(|u| u.as_str()) {
                        last_uuid = Some(uuid.to_string());
                    }
                }
            } else {
                tracing::trace!(line = line, "skipping unparseable claude transcript line");
            }
        }
        last_uuid
    }
}

/// Resolve claude's most-recently-modified transcript for `cwd` under an
/// explicit `home` directory. Production goes through the trait method which
/// reads `home` from `harness_home(Claude)` (env-var-aware). Tests call this
/// directly with a `TempDir` to avoid mutating process-global env vars.
pub fn latest_transcript_in(home: &Path, cwd: &Path) -> Option<PathBuf> {
    let projects_dir = home.join("projects");
    let escaped_cwd = cwd.to_string_lossy().replace('/', "-");
    let agent_dir = projects_dir.join(escaped_cwd);
    if !agent_dir.exists() {
        return None;
    }
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(&agent_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        match &newest {
            Some((_, prev)) if *prev >= mtime => {}
            _ => newest = Some((path, mtime)),
        }
    }
    newest.map(|(p, _)| p)
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

    for _ in 0..ctrl_c_count {
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

fn wait_for_pane_idle(session: &str, window: Option<&str>, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok((false, _)) = Tmux::is_agent_running_in(session, window) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}
