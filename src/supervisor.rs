//! Task agent session helpers.
//!
//! Tasks now own long-lived attached agents. This module keeps the tmux and
//! harness launch glue that task creation and the TUI need, but it no longer
//! does not implement staged task progression or sentinel polling.

use anyhow::{Context, Result};

use crate::agent_model::{AgentAttachment, AgentRecord, AgentStatus};
use crate::config::Config;
use crate::task::Task;
use crate::tmux::Tmux;

/// Legacy window name retained only for killing pre-redesign task panes during
/// migration/stop. New task sessions link canonical agent windows instead.
pub const AGMAN_WINDOW: &str = "agman";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollTarget {
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOutcome {
    Launched { session_name: String },
    Stopped,
}

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
            "task '{}' has no repos configured - cannot resolve tmux session",
            task.meta.task_id()
        )
    }
}

pub fn ensure_task_tmux(task: &Task) -> Result<String> {
    for repo in &task.meta.repos {
        Tmux::ensure_session(&repo.tmux_session, &repo.worktree_path).with_context(|| {
            format!(
                "failed to ensure tmux session for repo '{}'",
                repo.repo_name
            )
        })?;
    }
    if task.meta.is_multi_repo() {
        let parent_dir = task.meta.parent_dir.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "multi-repo task '{}' has no parent_dir",
                task.meta.task_id()
            )
        })?;
        let session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
        if !Tmux::session_exists(&session) {
            Tmux::create_session_with_windows(&session, parent_dir).with_context(|| {
                format!(
                    "failed to create parent-dir tmux session '{}' for multi-repo task",
                    session
                )
            })?;
        }
    }
    supervisor_session(task)
}

pub fn classify(_task: &Task) -> PollTarget {
    PollTarget::Skip
}

pub fn poll(_task: &Task) -> Result<PollOutcome> {
    Ok(PollOutcome::Idle)
}

pub fn kill_current_agent(harness: &dyn crate::harness::Harness, session_name: &str) -> Result<()> {
    harness.kill_pane(session_name, None)
}

pub fn launch_agent(config: &Config, task: &Task, agent: &AgentRecord) -> Result<String> {
    if agent.meta.status != AgentStatus::Running {
        anyhow::bail!("agent '{}' is archived", agent.meta.name);
    }
    match &agent.meta.attachment {
        AgentAttachment::Task { task_id, .. } if task_id == &task.meta.task_id() => {}
        _ => anyhow::bail!(
            "agent '{}' is not attached to task '{}'",
            agent.meta.name,
            task.meta.task_id()
        ),
    }

    crate::use_cases::start_agent_session(config, &agent.meta.project, &agent.meta.name, false)?;
    let session_name = crate::use_cases::agent_tmux_session_for_record(agent);
    if let Err(e) =
        crate::use_cases::link_agent_into_task_session(config, agent, &task.meta.task_id())
    {
        tracing::warn!(
            task_id = %task.meta.task_id(),
            agent = %agent.meta.name,
            error = %e,
            "failed to link agent window into task tmux session"
        );
    }
    Ok(session_name)
}

pub fn launch_task_engineer(config: &Config, task: &Task) -> Result<String> {
    let engineer = crate::use_cases::attached_engineer_for_task(config, &task.meta.task_id())?;
    launch_agent(config, task, &engineer)
}

pub fn launch_next_step(config: &Config, task: &mut Task) -> Result<AdvanceOutcome> {
    let session_name = launch_task_engineer(config, task)?;
    Ok(AdvanceOutcome::Launched { session_name })
}

pub fn honor_stop(config: &Config, task: &mut Task) -> Result<()> {
    if let Ok(engineer) = crate::use_cases::attached_engineer_for_task(config, &task.meta.task_id())
    {
        let session_name =
            Config::engineer_tmux_session(&engineer.meta.project, &engineer.meta.name);
        let harness = config.harness_kind().select();
        let _ = kill_current_agent(harness.as_ref(), &session_name);
    }
    Ok(())
}

pub fn wake_if_idle(config: &Config, task: &mut Task) -> Result<Option<AdvanceOutcome>> {
    Ok(Some(launch_next_step(config, task)?))
}

pub fn advance(_config: &Config, _task: &mut Task, _condition: ()) -> Result<AdvanceOutcome> {
    Ok(AdvanceOutcome::Stopped)
}
