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
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::agent::Agent;
use crate::command::StoredCommand;
use crate::config::Config;
use crate::flow::{Flow, FlowStep, LoopStep, StopCondition};
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

/// Per-tick classification of how the supervisor should treat a task.
///
/// Computed by `classify` from task meta and used by the TUI's background
/// poll loop to decide between polling a live session vs. relaunching a
/// half-state task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollTarget {
    /// Task has a live supervisor session; poll it for a stop condition.
    LiveSession { session_id: String },
    /// Task is Running but its last supervisor session already stopped —
    /// the caller should re-enter `launch_next_step` to recover. This is
    /// the "half-state" case (e.g. `wake_if_idle` drained the queue but a
    /// subsequent `launch_next_step` failed, or an `advance` relaunched
    /// and tmux was briefly unavailable).
    NeedsLaunch,
    /// Not a supervisor concern — either the task isn't Running, or it has
    /// no session history at all (still driven by `AgentRunner` today).
    Skip,
}

/// Decide how the supervisor's background poll should treat a task.
///
/// Classification rules (in order):
/// 1. Non-Running → `Skip`.
/// 2. Last session entry present and `stopped_at.is_none()` → `LiveSession`.
/// 3. Last session entry present and `stopped_at.is_some()` → `NeedsLaunch`
///    (half-state recovery — supervisor-driven task, ended without a relaunch).
/// 4. No session history → `Skip` (not yet supervised; `AgentRunner` owns it).
///
/// Keeping the "no history → Skip" branch is important during the transition
/// window when `AgentRunner::run_direct` is still the default driver for new
/// tasks. Once every task goes through the supervisor, rule 4 can be merged
/// into rule 3 (or removed) so that a first-ever launch failure also retries.
pub fn classify(task: &Task) -> PollTarget {
    if task.meta.status != TaskStatus::Running {
        return PollTarget::Skip;
    }
    match task.meta.session_history.last() {
        Some(entry) if entry.stopped_at.is_none() => PollTarget::LiveSession {
            session_id: entry.session_id.clone(),
        },
        Some(_) => PollTarget::NeedsLaunch,
        None => PollTarget::Skip,
    }
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
    /// Flow step is an unsupported shape. The supervisor is observation-only
    /// for this task; the legacy `AgentRunner` still drives it. Returned
    /// without mutating task state so callers can log and move on.
    Unsupported,
}

/// What to do next inside a `FlowStep::Loop` after an `AGENT_DONE`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopDecision {
    /// The loop's `until` matched — advance `flow_step` and reset sub-step.
    ExitLoop,
    /// Stay in the loop — set `flow_sub_step` to this index.
    NextSubStep(usize),
}

/// Pure decision function for loop progression on `AGENT_DONE`.
///
/// `TaskComplete` and `InputNeeded` are handled by `advance` directly and
/// never reach this function — they always propagate out of the loop.
fn decide_loop_next(loop_step: &LoopStep, sub_step: usize) -> LoopDecision {
    if loop_step.until == StopCondition::AgentDone {
        LoopDecision::ExitLoop
    } else {
        let n = loop_step.steps.len().max(1);
        LoopDecision::NextSubStep((sub_step + 1) % n)
    }
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

/// Resolve a `flow_name` stored on `TaskMeta` to a concrete YAML path.
///
/// Regular flows live in `flows_dir/<name>.yaml`; stored commands (drained
/// from `QueueItem::Command`) reuse their command id as the flow name and
/// live in `commands_dir/<id>.yaml`. Since command IDs and flow names share
/// a namespace inside `TaskMeta::flow_name`, we check the flows directory
/// first and fall back to the commands directory. The returned path always
/// points at an existing file — callers don't need to re-check.
fn resolve_flow_path(config: &Config, flow_name: &str) -> Result<PathBuf> {
    let flow_candidate = config.flow_path(flow_name);
    if flow_candidate.exists() {
        return Ok(flow_candidate);
    }
    let command_candidate = config.command_path(flow_name);
    if command_candidate.exists() {
        return Ok(command_candidate);
    }
    anyhow::bail!(
        "flow '{}' not found in flows_dir ({}) or commands_dir ({})",
        flow_name,
        flow_candidate.display(),
        command_candidate.display(),
    )
}

/// Drain the front of the queue at flow-end.
///
/// Returns `Ok(true)` if an item was processed (task state reset, supervisor
/// should attempt `launch_next_step` again with a freshly-loaded flow).
/// Returns `Ok(false)` if the queue is empty.
///
/// Handled kinds:
/// - `Feedback { text }` — write FEEDBACK.md, switch to the `continue` flow,
///   reset step/sub_step, set status=Running.
/// - `Command { command_id, branch }` — look up the `StoredCommand`; if it
///   requires a `branch` arg, write `.branch-target`. Switch `flow_name` to
///   the command id (resolved via `resolve_flow_path`), reset step/sub_step,
///   set status=Running. If the command doesn't exist on disk, log a warning
///   and drop the item without mutating task state so the supervisor doesn't
///   get stuck on a bad queue entry.
fn drain_queue(task: &mut Task, config: &Config) -> Result<bool> {
    if !task.has_queued_items() {
        return Ok(false);
    }
    let Some(item) = task.pop_queue()? else {
        return Ok(false);
    };
    match item {
        QueueItem::Feedback { text } => {
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
        QueueItem::Command { command_id, branch } => {
            let stored = match StoredCommand::get_by_id(&config.commands_dir, &command_id) {
                Ok(Some(cmd)) => cmd,
                Ok(None) => {
                    tracing::warn!(
                        task_id = %task.meta.task_id(),
                        command_id = %command_id,
                        "queued command not found on disk; dropping"
                    );
                    return Ok(false);
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.meta.task_id(),
                        command_id = %command_id,
                        error = %e,
                        "failed to load queued command; dropping"
                    );
                    return Ok(false);
                }
            };
            if stored.requires_arg.as_deref() == Some("branch") {
                if let Some(ref b) = branch {
                    std::fs::write(task.dir.join(".branch-target"), b)
                        .context("failed to write .branch-target")?;
                } else {
                    tracing::warn!(
                        task_id = %task.meta.task_id(),
                        command_id = %command_id,
                        "queued command requires a branch arg but none supplied; dropping"
                    );
                    return Ok(false);
                }
            }
            tracing::info!(
                task_id = %task.meta.task_id(),
                command_id = %command_id,
                "supervisor draining queued command at flow-end"
            );
            task.meta.flow_name = command_id;
            task.reset_flow_step()?;
            task.update_status(TaskStatus::Running)?;
            Ok(true)
        }
    }
}

/// Try to launch the agent at the current `flow_step`. If the current step is
/// an Agent with a successful pre_command, the step is skipped (plus any
/// post_hook) and the loop continues. On flow exhaustion the queue is drained
/// — `Feedback` resets the task to the `continue` flow and the loop restarts
/// with the freshly-loaded flow.
pub fn launch_next_step(config: &Config, task: &mut Task) -> Result<AdvanceOutcome> {
    // Guard against pathological loops (e.g. pre_command always passing and a
    // flow entirely made of pre_command steps).
    const MAX_ITERATIONS: usize = 32;
    for _ in 0..MAX_ITERATIONS {
        let flow_name = task.meta.flow_name.clone();
        let flow_path = resolve_flow_path(config, &flow_name)?;
        let flow = Flow::load(&flow_path)
            .with_context(|| format!("failed to load flow '{}'", flow_name))?;
        let step_index = task.meta.flow_step;
        match flow.get_step(step_index).cloned() {
            None => {
                task.update_status(TaskStatus::Stopped)?;
                task.update_agent(None)?;
                notify_pm(config, task, "Flow complete");
                if drain_queue(task, config)? {
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
            Some(FlowStep::Loop(loop_step)) => {
                if loop_step.steps.is_empty() {
                    tracing::warn!(
                        task_id = %task.meta.task_id(),
                        step = step_index,
                        "empty loop step; advancing past"
                    );
                    task.advance_flow_step()?;
                    continue;
                }
                let sub = task.meta.flow_sub_step.min(loop_step.steps.len() - 1);
                let inner = loop_step.steps[sub].clone();
                if let Some(ref cmd) = inner.pre_command {
                    match run_pre_command(task, cmd) {
                        Ok(true) => {
                            match decide_loop_next(&loop_step, sub) {
                                LoopDecision::ExitLoop => {
                                    task.advance_flow_step()?;
                                }
                                LoopDecision::NextSubStep(next) => {
                                    task.set_flow_sub_step(next)?;
                                }
                            }
                            continue;
                        }
                        Ok(false) => { /* fall through to agent */ }
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task.meta.task_id(),
                                error = %e,
                                "loop pre_command errored; falling through to agent"
                            );
                        }
                    }
                }
                let session_id = start_agent_step(config, task, &inner.agent)?;
                return Ok(AdvanceOutcome::Launched { session_id });
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
            if drain_queue(task, config)? {
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
            let flow_path = resolve_flow_path(config, &flow_name)?;
            let flow = Flow::load(&flow_path)
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
                Some(FlowStep::Loop(loop_step)) => {
                    // TaskComplete/InputNeeded are handled in branches above;
                    // only AgentDone reaches here. Consult `decide_loop_next`
                    // to see whether to exit the loop (when `loop.until` is
                    // AgentDone) or advance to the next inner agent.
                    match decide_loop_next(&loop_step, task.meta.flow_sub_step) {
                        LoopDecision::ExitLoop => {
                            task.advance_flow_step()?;
                        }
                        LoopDecision::NextSubStep(next) => {
                            task.set_flow_sub_step(next)?;
                        }
                    }
                    launch_next_step(config, task)
                }
                None => {
                    // Flow already past its last step — treat as flow-complete.
                    task.update_status(TaskStatus::Stopped)?;
                    task.update_agent(None)?;
                    notify_pm(config, task, "Flow complete");
                    if drain_queue(task, config)? {
                        return launch_next_step(config, task);
                    }
                    Ok(AdvanceOutcome::Stopped)
                }
            }
        }
    }
}

/// Wake a currently-Stopped task by draining the front of its queue and
/// launching the next agent step. No-op for non-Stopped tasks (nothing to
/// wake) and for Stopped tasks with an empty queue (nothing to drain).
///
/// Returns `Some(outcome)` when a launch was attempted, `None` otherwise.
///
/// This is the "idle drain" entry point used by `use_cases::queue_feedback`
/// and `use_cases::queue_command` to kick the supervisor immediately when
/// work is queued onto an idle task. The mid-flight drain (at flow-end on an
/// already-Running task) is handled inside `advance` / `launch_next_step`.
pub fn wake_if_idle(
    config: &Config,
    task: &mut Task,
) -> Result<Option<AdvanceOutcome>> {
    if task.meta.status != TaskStatus::Stopped {
        return Ok(None);
    }
    if !drain_queue(task, config)? {
        return Ok(None);
    }
    tracing::info!(
        task_id = %task.meta.task_id(),
        flow = %task.meta.flow_name,
        "supervisor waking idle task after queue drain"
    );
    Ok(Some(launch_next_step(config, task)?))
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
        let (cfg, mut task) = build_task(&tmp);
        task.queue_feedback("follow-up instructions").unwrap();
        task.meta.flow_name = "something_else".to_string();
        task.meta.flow_step = 5;
        task.meta.status = TaskStatus::Stopped;
        task.save_meta().unwrap();

        let drained = drain_queue(&mut task, &cfg).unwrap();
        assert!(drained);
        assert_eq!(task.meta.flow_name, "continue");
        assert_eq!(task.meta.flow_step, 0);
        assert_eq!(task.meta.status, TaskStatus::Running);
        let feedback = task.read_feedback().unwrap();
        assert_eq!(feedback, "follow-up instructions");
        assert!(!task.has_queued_items());
    }

    /// Helper: write a minimal StoredCommand YAML to `config.commands_dir`.
    fn write_test_command(config: &Config, id: &str, body: &str) {
        std::fs::write(config.command_path(id), body).unwrap();
    }

    #[test]
    fn drain_queue_command_switches_to_command_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut task) = build_task(&tmp);
        write_test_command(
            &cfg,
            "create-pr",
            "name: Create PR\nid: create-pr\ndescription: test\nsteps:\n  - agent: pr-creator\n    until: AGENT_DONE\n",
        );
        task.queue_command("create-pr", None).unwrap();
        task.meta.flow_name = "new".to_string();
        task.meta.flow_step = 3;
        task.meta.flow_sub_step = 1;
        task.meta.status = TaskStatus::Stopped;
        task.save_meta().unwrap();

        let drained = drain_queue(&mut task, &cfg).unwrap();
        assert!(drained);
        assert_eq!(task.meta.flow_name, "create-pr");
        assert_eq!(task.meta.flow_step, 0);
        assert_eq!(task.meta.flow_sub_step, 0);
        assert_eq!(task.meta.status, TaskStatus::Running);
        assert!(!task.has_queued_items());
        assert!(
            !task.dir.join(".branch-target").exists(),
            "no branch-target should be written for commands without branch arg"
        );
    }

    #[test]
    fn drain_queue_command_writes_branch_target_when_required() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut task) = build_task(&tmp);
        write_test_command(
            &cfg,
            "rebase",
            "name: Rebase\nid: rebase\ndescription: test\nrequires_arg: branch\nsteps:\n  - agent: rebaser\n    until: AGENT_DONE\n",
        );
        task.queue_command("rebase", Some("main")).unwrap();
        task.save_meta().unwrap();

        let drained = drain_queue(&mut task, &cfg).unwrap();
        assert!(drained);
        assert_eq!(task.meta.flow_name, "rebase");
        let branch_target = std::fs::read_to_string(task.dir.join(".branch-target")).unwrap();
        assert_eq!(branch_target, "main");
    }

    #[test]
    fn drain_queue_command_drops_missing_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut task) = build_task(&tmp);
        task.queue_command("does-not-exist", None).unwrap();
        task.meta.flow_name = "new".to_string();
        task.meta.flow_step = 2;
        task.save_meta().unwrap();

        let drained = drain_queue(&mut task, &cfg).unwrap();
        assert!(!drained, "missing command should be dropped");
        // State must be untouched so we don't corrupt flow position.
        assert_eq!(task.meta.flow_name, "new");
        assert_eq!(task.meta.flow_step, 2);
        assert!(!task.has_queued_items(), "bad item should be popped from queue");
    }

    #[test]
    fn drain_queue_command_drops_when_branch_missing_but_required() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut task) = build_task(&tmp);
        write_test_command(
            &cfg,
            "rebase",
            "name: Rebase\nid: rebase\ndescription: test\nrequires_arg: branch\nsteps:\n  - agent: rebaser\n    until: AGENT_DONE\n",
        );
        task.queue_command("rebase", None).unwrap();
        task.save_meta().unwrap();

        let drained = drain_queue(&mut task, &cfg).unwrap();
        assert!(!drained);
        assert!(!task.dir.join(".branch-target").exists());
    }

    #[test]
    fn resolve_flow_path_prefers_flows_dir_over_commands_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _task) = build_task(&tmp);
        // Both files exist — flow dir wins.
        std::fs::write(cfg.flow_path("shared"), "name: from-flows\nsteps: []\n").unwrap();
        std::fs::write(cfg.command_path("shared"), "name: from-commands\nsteps: []\n").unwrap();

        let resolved = resolve_flow_path(&cfg, "shared").unwrap();
        assert_eq!(resolved, cfg.flow_path("shared"));
    }

    #[test]
    fn resolve_flow_path_falls_back_to_commands_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _task) = build_task(&tmp);
        std::fs::write(cfg.command_path("create-pr"), "name: cmd\nsteps: []\n").unwrap();

        let resolved = resolve_flow_path(&cfg, "create-pr").unwrap();
        assert_eq!(resolved, cfg.command_path("create-pr"));
    }

    #[test]
    fn resolve_flow_path_errors_when_nowhere() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _task) = build_task(&tmp);
        assert!(resolve_flow_path(&cfg, "missing").is_err());
    }

    // -----------------------------------------------------------------------
    // Loop-support tests (FlowStep::Loop)
    // -----------------------------------------------------------------------

    fn make_loop(steps: Vec<crate::flow::AgentStep>, until: StopCondition) -> LoopStep {
        LoopStep { steps, until }
    }

    fn make_agent_step(name: &str, until: StopCondition) -> crate::flow::AgentStep {
        crate::flow::AgentStep {
            agent: name.to_string(),
            until,
            on_fail: None,
            post_hook: None,
            pre_command: None,
        }
    }

    #[test]
    fn decide_loop_next_advances_sub_step_when_until_is_task_complete() {
        let loop_step = make_loop(
            vec![
                make_agent_step("coder", StopCondition::AgentDone),
                make_agent_step("checker", StopCondition::AgentDone),
            ],
            StopCondition::TaskComplete,
        );
        assert_eq!(decide_loop_next(&loop_step, 0), LoopDecision::NextSubStep(1));
    }

    #[test]
    fn decide_loop_next_wraps_sub_step_at_end_of_loop() {
        let loop_step = make_loop(
            vec![
                make_agent_step("coder", StopCondition::AgentDone),
                make_agent_step("checker", StopCondition::AgentDone),
            ],
            StopCondition::TaskComplete,
        );
        assert_eq!(decide_loop_next(&loop_step, 1), LoopDecision::NextSubStep(0));
    }

    #[test]
    fn decide_loop_next_exits_when_until_is_agent_done() {
        let loop_step = make_loop(
            vec![make_agent_step("coder", StopCondition::AgentDone)],
            StopCondition::AgentDone,
        );
        assert_eq!(decide_loop_next(&loop_step, 0), LoopDecision::ExitLoop);
    }

    #[test]
    fn advance_flow_step_resets_sub_step() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        task.meta.flow_step = 0;
        task.meta.flow_sub_step = 5;
        task.save_meta().unwrap();

        task.advance_flow_step().unwrap();
        assert_eq!(task.meta.flow_step, 1);
        assert_eq!(task.meta.flow_sub_step, 0);
    }

    #[test]
    fn reset_flow_step_resets_sub_step() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        task.meta.flow_step = 3;
        task.meta.flow_sub_step = 2;
        task.save_meta().unwrap();

        task.reset_flow_step().unwrap();
        assert_eq!(task.meta.flow_step, 0);
        assert_eq!(task.meta.flow_sub_step, 0);
    }

    #[test]
    fn advance_loop_exits_when_until_agent_done_matches() {
        // Single-step flow whose only step is a loop with `until: AGENT_DONE`.
        // Receiving AgentDone exits the loop, advances flow_step past the end,
        // and stops the task — no tmux launch needed.
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        write_test_flow(
            &config,
            "test_loop_exit",
            "name: test_loop_exit\nsteps:\n  - loop:\n      - agent: coder\n        until: AGENT_DONE\n    until: AGENT_DONE\n",
        );
        task.meta.flow_name = "test_loop_exit".to_string();
        task.meta.flow_step = 0;
        task.meta.flow_sub_step = 0;
        task.save_meta().unwrap();
        push_session(&mut task, "coder", "sid-1");

        let outcome = advance(&config, &mut task, StopCondition::AgentDone).unwrap();
        assert_eq!(outcome, AdvanceOutcome::Stopped);
        assert_eq!(task.meta.flow_step, 1, "flow_step should advance past loop");
        assert_eq!(task.meta.flow_sub_step, 0, "sub_step should reset on loop exit");
        assert_eq!(task.meta.status, TaskStatus::Stopped);
    }

    #[test]
    fn advance_loop_task_complete_stops_regardless_of_sub_step() {
        // Loop with `until: TASK_COMPLETE` (mirrors new.yaml). When any inner
        // agent returns TASK_COMPLETE, the task stops — sub_step position
        // doesn't matter.
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        write_test_flow(
            &config,
            "test_loop_tc",
            "name: test_loop_tc\nsteps:\n  - loop:\n      - agent: coder\n        until: AGENT_DONE\n      - agent: checker\n        until: AGENT_DONE\n    until: TASK_COMPLETE\n",
        );
        task.meta.flow_name = "test_loop_tc".to_string();
        task.meta.flow_step = 0;
        task.meta.flow_sub_step = 1;
        task.save_meta().unwrap();
        push_session(&mut task, "checker", "sid-checker");

        let outcome = advance(&config, &mut task, StopCondition::TaskComplete).unwrap();
        assert_eq!(outcome, AdvanceOutcome::Stopped);
        assert_eq!(task.meta.status, TaskStatus::Stopped);
    }

    #[test]
    fn advance_loop_input_needed_pauses_without_changing_sub_step() {
        // InputNeeded mid-loop should leave flow_step + flow_sub_step untouched
        // so the answerer re-enters the same inner agent.
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        write_test_flow(
            &config,
            "test_loop_in",
            "name: test_loop_in\nsteps:\n  - loop:\n      - agent: coder\n        until: AGENT_DONE\n      - agent: checker\n        until: AGENT_DONE\n    until: TASK_COMPLETE\n",
        );
        task.meta.flow_name = "test_loop_in".to_string();
        task.meta.flow_step = 0;
        task.meta.flow_sub_step = 1;
        task.save_meta().unwrap();
        push_session(&mut task, "checker", "sid-checker");

        let outcome = advance(&config, &mut task, StopCondition::InputNeeded).unwrap();
        assert_eq!(outcome, AdvanceOutcome::InputNeeded);
        assert_eq!(task.meta.status, TaskStatus::InputNeeded);
        assert_eq!(task.meta.flow_step, 0);
        assert_eq!(task.meta.flow_sub_step, 1);
    }

    // -----------------------------------------------------------------------
    // wake_if_idle — idle-drain entry point
    // -----------------------------------------------------------------------

    #[test]
    fn wake_if_idle_noop_on_running_task() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        // task starts Running; queue an item to ensure drain is not attempted.
        task.queue_feedback("pending").unwrap();
        task.save_meta().unwrap();

        let result = wake_if_idle(&config, &mut task).unwrap();
        assert!(result.is_none(), "running task should not be woken");
        assert_eq!(task.meta.status, TaskStatus::Running);
        assert!(task.has_queued_items(), "queue must not be drained");
    }

    #[test]
    fn wake_if_idle_noop_on_stopped_empty_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        task.update_status(TaskStatus::Stopped).unwrap();

        let result = wake_if_idle(&config, &mut task).unwrap();
        assert!(result.is_none(), "nothing to drain, no launch attempted");
        assert_eq!(task.meta.status, TaskStatus::Stopped);
    }

    #[test]
    fn wake_if_idle_drains_feedback_on_stopped_task() {
        // Stopped + queued feedback → drain resets to `continue` flow.
        // The subsequent launch_next_step tries to start `refiner` via tmux
        // which fails in the test env; we assert that the drain side-effects
        // (flow_name, status, FEEDBACK.md) were persisted before the error.
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut task) = build_task(&tmp);
        task.queue_feedback("please fix the bug").unwrap();
        task.meta.flow_name = "new".to_string();
        task.meta.flow_step = 3;
        task.meta.status = TaskStatus::Stopped;
        task.save_meta().unwrap();

        // Error propagates from launch_next_step; we don't care about the Ok/Err
        // result here — we only care that drain_queue ran first.
        let _ = wake_if_idle(&config, &mut task);

        assert_eq!(task.meta.flow_name, "continue");
        assert_eq!(task.meta.flow_step, 0);
        assert_eq!(task.meta.status, TaskStatus::Running);
        let fb = task.read_feedback().unwrap();
        assert_eq!(fb, "please fix the bug");
        assert!(!task.has_queued_items());
    }

    // -----------------------------------------------------------------------
    // classify — supervisor poll task triage
    // -----------------------------------------------------------------------

    #[test]
    fn classify_skips_non_running_task() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        task.meta.status = TaskStatus::Stopped;
        // Non-Running takes precedence over any session state.
        push_session(&mut task, "coder", "sid-1");

        assert_eq!(classify(&task), PollTarget::Skip);
    }

    #[test]
    fn classify_skips_running_without_session_history() {
        // A Running task with no session_history entries is still owned by
        // `AgentRunner` during the transition — supervisor must not touch it.
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, task) = build_task(&tmp);
        assert_eq!(task.meta.status, TaskStatus::Running);
        assert!(task.meta.session_history.is_empty());

        assert_eq!(classify(&task), PollTarget::Skip);
    }

    #[test]
    fn classify_live_session_returns_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        push_session(&mut task, "coder", "sid-live");

        assert_eq!(
            classify(&task),
            PollTarget::LiveSession {
                session_id: "sid-live".to_string()
            }
        );
    }

    #[test]
    fn classify_needs_launch_when_last_session_stopped() {
        // Half-state: Running + last session has stopped_at=Some. This is the
        // post-wake_if_idle or post-advance failure scenario the new poll
        // branch is designed to recover.
        let tmp = tempfile::tempdir().unwrap();
        let (_cfg, mut task) = build_task(&tmp);
        push_session(&mut task, "coder", "sid-done");
        task.finish_last_session(Some("AGENT_DONE".to_string())).unwrap();

        assert_eq!(classify(&task), PollTarget::NeedsLaunch);
    }
}
