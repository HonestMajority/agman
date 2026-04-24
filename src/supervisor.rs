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
use std::process::{Command, Stdio};

use crate::agent::Agent;
use crate::config::Config;
use crate::flow::{Flow, FlowStep, StopCondition};
use crate::task::{QueueItem, SessionEntry, Task, TaskStatus};
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

// ---------------------------------------------------------------------------
// State machine: advance()
// ---------------------------------------------------------------------------

/// Outcome of a single `advance` call. Lets the caller log or trigger
/// follow-up work (e.g. refresh the TUI task list after a status change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOutcome {
    /// The supervisor launched a new claude session; the poller will pick it up.
    Launched { session_id: String },
    /// Task entered a waiting state (input needed from the user).
    InputNeeded,
    /// Flow finished and no queued feedback/command replaced it. Task is Stopped.
    Stopped,
    /// Flow step is an unsupported shape (currently `Loop`). The supervisor is
    /// observation-only for this task; the legacy `AgentRunner` still drives it.
    /// Returned without mutating task state so callers can log and move on.
    Unsupported,
}

/// Determine the tmux session's working directory. Multi-repo tasks use
/// `parent_dir`; single-repo tasks use the primary repo's worktree.
fn step_working_dir(task: &Task) -> Result<std::path::PathBuf> {
    if task.meta.is_multi_repo() {
        match task.meta.parent_dir.as_deref() {
            Some(parent) => Ok(parent.to_path_buf()),
            None => anyhow::bail!(
                "multi-repo task '{}' has no parent_dir",
                task.meta.task_id()
            ),
        }
    } else if task.meta.has_repos() {
        Ok(task.meta.primary_repo().worktree_path.clone())
    } else {
        anyhow::bail!(
            "task '{}' has no repos configured",
            task.meta.task_id()
        )
    }
}

/// Run a step's pre_command. Returns `Ok(true)` on exit 0 (skip agent),
/// `Ok(false)` if the command failed (fall through to running the agent).
fn run_pre_command(task: &Task, cmd: &str) -> Result<bool> {
    let working_dir = step_working_dir(task)?;
    tracing::info!(
        task_id = %task.meta.task_id(),
        pre_command = cmd,
        "supervisor running pre_command"
    );
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&working_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to execute pre_command: {}", cmd))?;
    Ok(status.success())
}

/// Execute a named post-hook after an agent step completes.
fn run_post_hook(config: &Config, task: &mut Task, hook: &str) -> Result<()> {
    match hook {
        "setup_repos" => {
            tracing::info!(task_id = %task.meta.task_id(), "supervisor executing setup_repos post-hook");
            crate::use_cases::setup_repos_from_task_md(config, task, false)?;
            Ok(())
        }
        _ => {
            tracing::warn!(task_id = %task.meta.task_id(), hook = hook, "unknown post-hook, ignoring");
            Ok(())
        }
    }
}

/// Scan all known worktree roots + parent_dir for a `.pr-link` sidecar file.
/// Format: two lines — PR number on line 1, URL on line 2. Parses and calls
/// `task.set_linked_pr`, then removes the sidecar file. No-op when absent.
fn detect_pr_link(task: &mut Task) -> Result<()> {
    let mut candidates: Vec<std::path::PathBuf> = task
        .meta
        .repos
        .iter()
        .map(|r| r.worktree_path.join(".pr-link"))
        .collect();
    if let Some(ref parent) = task.meta.parent_dir {
        candidates.push(parent.join(".pr-link"));
    }
    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(path) else { continue };
        let lines: Vec<&str> = contents.lines().collect();
        if lines.len() >= 2 {
            if let Ok(number) = lines[0].trim().parse::<u64>() {
                let url = lines[1].trim().to_string();
                tracing::info!(
                    task_id = %task.meta.task_id(),
                    pr_number = number,
                    pr_url = %url,
                    "detected .pr-link, storing linked PR"
                );
                let _ = task.set_linked_pr(number, url, true, None);
            }
        }
        let _ = std::fs::remove_file(path);
        break;
    }
    Ok(())
}

/// Best-effort notify the owning PM via the existing send_message path.
/// Errors are logged but not propagated.
fn notify_pm(config: &Config, task: &Task, message: &str) {
    if let Some(ref project) = task.meta.project {
        if let Err(e) = crate::use_cases::send_message(
            config,
            project,
            &task.meta.task_id(),
            message,
        ) {
            tracing::warn!(
                task_id = %task.meta.task_id(),
                project = %project,
                error = %e,
                "failed to notify PM"
            );
        }
    }
}

/// Drain the front of the queue at flow-end.
///
/// Returns `Ok(true)` if an item was processed (task state reset, supervisor
/// should attempt `launch_next_step` again with a freshly-loaded flow).
/// Returns `Ok(false)` if the queue is empty or the front item can't be
/// handled by the supervisor yet (currently: any `Command` entry is left in
/// place for the TUI/AgentRunner path until stored-command dispatch lands).
fn drain_queue(task: &mut Task) -> Result<bool> {
    if !task.has_queued_items() {
        return Ok(false);
    }
    let queue = task.read_queue();
    match queue.first() {
        Some(QueueItem::Feedback { .. }) => {}
        _ => return Ok(false),
    }
    let Some(QueueItem::Feedback { text }) = task.pop_queue()? else {
        return Ok(false);
    };
    tracing::info!(
        task_id = %task.meta.task_id(),
        "supervisor draining queued feedback at flow-end"
    );
    task.write_feedback(&text)?;
    task.meta.flow_name = "continue".to_string();
    task.reset_flow_step()?;
    task.update_status(TaskStatus::Running)?;
    Ok(true)
}

/// Try to launch the agent at the current `flow_step`. If the current step is
/// an Agent with a successful pre_command, the step is skipped (plus any
/// post_hook) and the loop continues. On flow exhaustion the queue is drained
/// — `Feedback` resets the task to the `continue` flow and the loop restarts
/// with the freshly-loaded flow.
fn launch_next_step(config: &Config, task: &mut Task) -> Result<AdvanceOutcome> {
    // Guard against pathological loops (e.g. pre_command always passing and a
    // flow entirely made of pre_command steps).
    const MAX_ITERATIONS: usize = 32;
    for _ in 0..MAX_ITERATIONS {
        let flow_name = task.meta.flow_name.clone();
        let flow = Flow::load(&config.flow_path(&flow_name))
            .with_context(|| format!("failed to load flow '{}'", flow_name))?;
        let step_index = task.meta.flow_step;
        match flow.get_step(step_index).cloned() {
            None => {
                task.update_status(TaskStatus::Stopped)?;
                task.update_agent(None)?;
                notify_pm(config, task, "Flow complete");
                if drain_queue(task)? {
                    // Re-enter with the new flow.
                    continue;
                }
                return Ok(AdvanceOutcome::Stopped);
            }
            Some(FlowStep::Agent(agent_step)) => {
                if let Some(ref cmd) = agent_step.pre_command {
                    match run_pre_command(task, cmd) {
                        Ok(true) => {
                            if agent_step.until == StopCondition::AgentDone {
                                if let Some(ref hook) = agent_step.post_hook {
                                    run_post_hook(config, task, hook)?;
                                }
                                task.advance_flow_step()?;
                                continue;
                            }
                        }
                        Ok(false) => { /* fall through to agent */ }
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task.meta.task_id(),
                                error = %e,
                                "pre_command errored; falling through to agent"
                            );
                        }
                    }
                }
                let session_id = start_agent_step(config, task, &agent_step.agent)?;
                return Ok(AdvanceOutcome::Launched { session_id });
            }
            Some(FlowStep::Loop(_)) => {
                // Loop steps are not yet handled by the supervisor; the legacy
                // AgentRunner still drives flows that contain loops. Left in
                // place as a TODO for the next iteration.
                tracing::info!(
                    task_id = %task.meta.task_id(),
                    step = step_index,
                    "supervisor encountered loop step; deferring to legacy runner"
                );
                return Ok(AdvanceOutcome::Unsupported);
            }
        }
    }
    anyhow::bail!(
        "launch_next_step exceeded max iterations for task '{}'",
        task.meta.task_id()
    )
}

/// Apply a detected stop condition to a task and, if appropriate, launch the
/// next step. This is the supervisor's state-machine entry point, called from
/// the TUI once `poll` returns `PollOutcome::Condition(_)`.
///
/// Side effects:
/// - Records the condition on the most recent `SessionEntry`.
/// - If the last agent was `refiner`, clears FEEDBACK.md.
/// - Scans for `.pr-link` sidecar and persists the linked PR.
/// - Updates status / advances flow step based on the condition.
/// - Notifies the owning PM of terminal states.
/// - Launches the next agent (with pre_command + post_hook support).
pub fn advance(
    config: &Config,
    task: &mut Task,
    condition: StopCondition,
) -> Result<AdvanceOutcome> {
    tracing::info!(
        task_id = %task.meta.task_id(),
        condition = %condition,
        "supervisor advance"
    );

    // 1. Stamp the condition on the most recent session.
    task.finish_last_session(Some(condition.to_string()))?;

    // 2. Post-step cleanup.
    let last_agent = task
        .meta
        .session_history
        .last()
        .map(|s| s.agent.clone());
    if last_agent.as_deref() == Some("refiner") {
        task.clear_feedback()?;
    }
    detect_pr_link(task)?;

    // 3. Dispatch on the condition.
    match condition {
        StopCondition::TaskComplete => {
            task.update_status(TaskStatus::Stopped)?;
            task.update_agent(None)?;
            notify_pm(config, task, "Task complete");
            if drain_queue(task)? {
                return launch_next_step(config, task);
            }
            Ok(AdvanceOutcome::Stopped)
        }
        StopCondition::InputNeeded => {
            task.update_status(TaskStatus::InputNeeded)?;
            notify_pm(config, task, "Task needs input");
            // Do NOT advance — re-run same agent after user answers.
            Ok(AdvanceOutcome::InputNeeded)
        }
        StopCondition::AgentDone => {
            // If the current step is an Agent whose `until == AgentDone`,
            // advance. Otherwise the step needs another pass with the same
            // agent (e.g. a pre_command that wants TASK_COMPLETE).
            let flow_name = task.meta.flow_name.clone();
            let flow = Flow::load(&config.flow_path(&flow_name))
                .with_context(|| format!("failed to load flow '{}'", flow_name))?;
            let step_index = task.meta.flow_step;
            match flow.get_step(step_index).cloned() {
                Some(FlowStep::Agent(agent_step)) => {
                    if agent_step.until == StopCondition::AgentDone {
                        if let Some(ref hook) = agent_step.post_hook {
                            run_post_hook(config, task, hook)?;
                        }
                        task.advance_flow_step()?;
                    }
                    launch_next_step(config, task)
                }
                Some(FlowStep::Loop(_)) => {
                    // Supervisor doesn't drive loops yet; observation-only.
                    Ok(AdvanceOutcome::Unsupported)
                }
                None => {
                    // Flow already past its last step — treat as flow-complete.
                    task.update_status(TaskStatus::Stopped)?;
                    task.update_agent(None)?;
                    notify_pm(config, task, "Flow complete");
                    if drain_queue(task)? {
                        return launch_next_step(config, task);
                    }
                    Ok(AdvanceOutcome::Stopped)
                }
            }
        }
    }
}

/// Honor a `.stop` sentinel: kill the currently-running claude, finish the
/// live session, and transition the task to Stopped. Idempotent — safe to
/// call even if the session is already stopped or the sentinel is gone.
pub fn honor_stop(config: &Config, task: &mut Task) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "supervisor honoring stop");
    let _ = config; // reserved for future notify/cleanup use
    if let Ok(session) = supervisor_session(task) {
        let _ = kill_current_claude(&session);
    }
    task.clear_stop()?;
    task.clear_agent_done()?;
    task.finish_last_session(Some("STOPPED".to_string()))?;
    task.update_agent(None)?;
    task.update_status(TaskStatus::Stopped)?;
    Ok(())
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

    /// Helper: push a session onto history so `finish_last_session` has
    /// something to stamp.
    fn push_session(task: &mut Task, agent: &str, session_id: &str) {
        task.push_session(SessionEntry {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            started_at: chrono::Utc::now(),
            stopped_at: None,
            condition: None,
        })
        .unwrap();
    }

    /// Helper: write a minimal single-step flow yaml to `config.flow_path(name)`.
    fn write_test_flow(config: &Config, name: &str, yaml: &str) {
        std::fs::write(config.flow_path(name), yaml).unwrap();
    }

    #[test]
    fn advance_task_complete_transitions_to_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        push_session(&mut task, "coder", "sid-1");

        let outcome = advance(&config, &mut task, StopCondition::TaskComplete).unwrap();
        assert_eq!(outcome, AdvanceOutcome::Stopped);
        assert_eq!(task.meta.status, TaskStatus::Stopped);
        let last = task.meta.session_history.last().unwrap();
        assert_eq!(last.condition.as_deref(), Some("TASK_COMPLETE"));
        assert!(last.stopped_at.is_some());
    }

    #[test]
    fn advance_input_needed_transitions_without_advancing_step() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        task.meta.flow_step = 0;
        task.save_meta().unwrap();
        push_session(&mut task, "coder", "sid-1");

        let outcome = advance(&config, &mut task, StopCondition::InputNeeded).unwrap();
        assert_eq!(outcome, AdvanceOutcome::InputNeeded);
        assert_eq!(task.meta.status, TaskStatus::InputNeeded);
        // Did not advance the step — answerer will re-enter the same agent.
        assert_eq!(task.meta.flow_step, 0);
    }

    #[test]
    fn advance_agent_done_past_last_step_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        write_test_flow(
            &config,
            "test_single",
            "name: test_single\nsteps:\n  - agent: coder\n    until: AGENT_DONE\n",
        );
        task.meta.flow_name = "test_single".to_string();
        task.meta.flow_step = 0;
        task.save_meta().unwrap();
        push_session(&mut task, "coder", "sid-1");

        let outcome = advance(&config, &mut task, StopCondition::AgentDone).unwrap();
        assert_eq!(outcome, AdvanceOutcome::Stopped);
        assert_eq!(task.meta.flow_step, 1);
        assert_eq!(task.meta.status, TaskStatus::Stopped);
    }

    #[test]
    fn advance_clears_feedback_after_refiner() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        task.write_feedback("pending feedback").unwrap();
        assert!(task.dir.join("FEEDBACK.md").exists());
        push_session(&mut task, "refiner", "sid-refiner");

        let _ = advance(&config, &mut task, StopCondition::TaskComplete).unwrap();
        assert!(
            !task.dir.join("FEEDBACK.md").exists(),
            "FEEDBACK.md should be removed after refiner step completes"
        );
    }

    #[test]
    fn advance_detects_pr_link_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        std::fs::create_dir_all(&task.meta.repos[0].worktree_path).unwrap();
        let pr_link = task.meta.repos[0].worktree_path.join(".pr-link");
        std::fs::write(&pr_link, "4242\nhttps://example.com/pr/4242\n").unwrap();
        push_session(&mut task, "coder", "sid-1");

        let _ = advance(&config, &mut task, StopCondition::TaskComplete).unwrap();

        let linked = task.meta.linked_pr.as_ref().expect("linked_pr should be set");
        assert_eq!(linked.number, 4242);
        assert_eq!(linked.url, "https://example.com/pr/4242");
        assert!(!pr_link.exists(), ".pr-link sidecar should be consumed");
    }

    #[test]
    fn drain_queue_feedback_resets_to_continue_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        task.queue_feedback("follow-up instructions").unwrap();
        task.meta.flow_name = "something_else".to_string();
        task.meta.flow_step = 5;
        task.meta.status = TaskStatus::Stopped;
        task.save_meta().unwrap();

        let drained = drain_queue(&mut task).unwrap();
        assert!(drained);
        assert_eq!(task.meta.flow_name, "continue");
        assert_eq!(task.meta.flow_step, 0);
        assert_eq!(task.meta.status, TaskStatus::Running);
        let feedback = task.read_feedback().unwrap();
        assert_eq!(feedback, "follow-up instructions");
        assert!(!task.has_queued_items());
    }

    #[test]
    fn drain_queue_skips_when_front_is_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        task.queue_command("some-command", None).unwrap();

        let drained = drain_queue(&mut task).unwrap();
        assert!(!drained, "Command entries are left for the TUI/legacy path");
        assert!(task.has_queued_items(), "queue should be untouched");
    }
}
