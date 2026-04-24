//! Task supervisor: runs agent flow steps as interactive `claude` sessions
//! inside a tmux window (the task session's `agman` window) and advances the
//! flow based on sentinel files + pane scanning.
//!
//! This module replaces the old `AgentRunner` background-subprocess model.
//! Each step launches a fresh interactive `claude --system-prompt ...
//! --session-id <uuid>`; the supervisor polls for `<task_dir>/.agent-done`
//! (written by the agent as its last action) or, as a fallback, scans the
//! pane for `AGENT_DONE:<uuid>` / `TASK_COMPLETE:<uuid>` / `INPUT_NEEDED:<uuid>`.
//!
//! The supervisor is intentionally built around one-shot tick operations so
//! the TUI main loop can drive it the same way it drives the inbox poller.

use anyhow::{Context, Result};
use std::process::Command;

use crate::agent::Agent;
use crate::config::Config;
use crate::flow::StopCondition;
use crate::task::{SessionEntry, Task};
use crate::tmux::Tmux;

/// Window name inside a task's tmux session that hosts the interactive
/// `claude` session driven by the supervisor.
pub const AGMAN_WINDOW: &str = "agman";

/// Resolve the tmux session that hosts the supervisor's `agman` window for a
/// task. Multi-repo tasks use the parent-directory session; single-repo
/// tasks use the primary repo's session.
pub fn supervisor_session(task: &Task) -> Result<String> {
    if task.meta.is_multi_repo() {
        Ok(Config::tmux_session_name(
            &task.meta.name,
            &task.meta.branch_name,
        ))
    } else if task.meta.has_repos() {
        Ok(task.meta.primary_repo().tmux_session.clone())
    } else {
        anyhow::bail!(
            "task '{}' has no repos configured — cannot resolve tmux session",
            task.meta.task_id()
        )
    }
}

/// Outcome of a single supervisor poll cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// Nothing interesting happened — claude is still running.
    Idle,
    /// The user wrote `.stop`; the supervisor should kill claude and halt.
    StopRequested,
    /// The agent reported a stop condition via `.agent-done` or pane scan.
    Condition(StopCondition),
}

/// Kill the `claude` process currently running in a session's `agman` window,
/// best-effort. Tries SIGTERM on the pane pid; falls back to two Ctrl+C key
/// presses if the pid is not available.
pub fn kill_current_claude(session_name: &str) -> Result<()> {
    match Tmux::pane_pid(session_name, Some(AGMAN_WINDOW))? {
        Some(pid) => {
            tracing::debug!(session = session_name, pid, "SIGTERM-ing pane pid");
            let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).output();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        None => {
            tracing::debug!(session = session_name, "no pane pid; sending Ctrl+C x2");
            let _ = Tmux::send_ctrl_c_to_window(session_name, AGMAN_WINDOW);
            std::thread::sleep(std::time::Duration::from_millis(150));
            let _ = Tmux::send_ctrl_c_to_window(session_name, AGMAN_WINDOW);
        }
    }
    Ok(())
}

/// Build and return the system-prompt for a step, writing it to
/// `<task_dir>/.current-prompt.md` so `claude --system-prompt "$(cat ...)"`
/// can pick it up.
pub fn prepare_prompt(config: &Config, task: &Task, agent_name: &str) -> Result<String> {
    let agent = Agent::load(config, agent_name)?;
    let prompt = agent.build_prompt(task)?;
    std::fs::write(task.current_prompt_path(), &prompt)
        .context("failed to write .current-prompt.md")?;
    Ok(prompt)
}

/// Build the shell command that launches interactive `claude` in the agman
/// window with the prepared system prompt and a pinned `--session-id`.
pub fn claude_launch_cmd(task: &Task, session_id: &str) -> String {
    format!(
        "claude --dangerously-skip-permissions --session-id {} --system-prompt \"$(cat {})\"",
        session_id,
        task.current_prompt_path().display()
    )
}

/// Start an agent step as an interactive claude session.
///
/// Sequence:
/// 1. Kill any prior claude running in the agman window
/// 2. Clear stale `.agent-done`
/// 3. Build the prompt + write `.current-prompt.md`
/// 4. Append a `SessionEntry` to `meta.session_history`
/// 5. `send-keys` the claude launch command to `<session>:agman`
///
/// Returns the newly-minted `session_id` so callers can log or resume.
pub fn start_agent_step(
    config: &Config,
    task: &mut Task,
    agent_name: &str,
) -> Result<String> {
    let session_name = supervisor_session(task)?;

    kill_current_claude(&session_name)?;
    task.clear_agent_done()?;
    prepare_prompt(config, task, agent_name)?;

    let session_id = new_session_id();
    let entry = SessionEntry {
        agent: agent_name.to_string(),
        session_id: session_id.clone(),
        started_at: chrono::Utc::now(),
        stopped_at: None,
        condition: None,
    };
    task.push_session(entry)?;
    task.update_agent(Some(agent_name.to_string()))?;

    let cmd = claude_launch_cmd(task, &session_id);
    Tmux::send_keys_to_window(&session_name, AGMAN_WINDOW, &cmd)?;

    tracing::info!(
        task_id = %task.meta.task_id(),
        agent = agent_name,
        session_id = %session_id,
        "supervisor launched agent step"
    );

    Ok(session_id)
}

/// One supervisor tick. Reads `.stop` first, then `.agent-done`, then falls
/// back to scanning the tmux pane for `AGENT_DONE:<session_id>` /
/// `TASK_COMPLETE:<session_id>` / `INPUT_NEEDED:<session_id>`.
pub fn poll(task: &Task, session_id: &str) -> Result<PollOutcome> {
    if task.stop_requested() {
        return Ok(PollOutcome::StopRequested);
    }

    if let Some(raw) = task.take_agent_done()? {
        if let Some(cond) = StopCondition::from_output(&raw) {
            return Ok(PollOutcome::Condition(cond));
        }
        tracing::warn!(
            task_id = %task.meta.task_id(),
            raw = %raw,
            "unparseable .agent-done sentinel; ignoring"
        );
    }

    if let Ok(session_name) = supervisor_session(task) {
        let pane = Tmux::capture_pane_window(&session_name, Some(AGMAN_WINDOW))
            .unwrap_or_default();
        for magic in ["AGENT_DONE", "TASK_COMPLETE", "INPUT_NEEDED"] {
            let needle = format!("{}:{}", magic, session_id);
            if pane.contains(&needle) {
                if let Some(cond) = StopCondition::from_output(magic) {
                    return Ok(PollOutcome::Condition(cond));
                }
            }
        }
    }

    Ok(PollOutcome::Idle)
}

/// Generate a fresh claude `--session-id` as a v4 UUID.
fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let id = new_session_id();
            assert!(seen.insert(id), "duplicate session id");
        }
    }

    fn build_task(tmp: &tempfile::TempDir) -> (Config, Task) {
        let config = Config::new(tmp.path().join(".agman"), tmp.path().join("repos"));
        config.ensure_dirs().unwrap();
        let dir = config.task_dir("r", "b");
        std::fs::create_dir_all(&dir).unwrap();
        let task = Task {
            meta: crate::task::TaskMeta::new(
                "r".into(),
                "b".into(),
                config.worktree_path("r", "b"),
                "new".into(),
            ),
            dir,
        };
        task.save_meta().unwrap();
        (config, task)
    }

    #[test]
    fn poll_detects_stop_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, task) = build_task(&tmp);
        task.request_stop().unwrap();
        assert_eq!(poll(&task, "sid").unwrap(), PollOutcome::StopRequested);
    }

    #[test]
    fn poll_reads_agent_done_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, task) = build_task(&tmp);
        std::fs::write(task.agent_done_path(), "AGENT_DONE\n").unwrap();
        assert_eq!(
            poll(&task, "sid").unwrap(),
            PollOutcome::Condition(StopCondition::AgentDone)
        );
        // Sentinel is consumed
        assert!(!task.agent_done_path().exists());
    }

    #[test]
    fn poll_reads_task_complete_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, task) = build_task(&tmp);
        std::fs::write(task.agent_done_path(), "TASK_COMPLETE").unwrap();
        assert_eq!(
            poll(&task, "sid").unwrap(),
            PollOutcome::Condition(StopCondition::TaskComplete)
        );
    }

    #[test]
    fn claude_launch_cmd_references_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, task) = build_task(&tmp);
        let cmd = claude_launch_cmd(&task, "abc-123");
        assert!(cmd.contains("--session-id abc-123"));
        assert!(cmd.contains(".current-prompt.md"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(cmd.contains("--system-prompt"));
    }
}
