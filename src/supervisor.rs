//! Task agent session helpers.
//!
//! Tasks own long-lived attached agents. This module keeps the tmux and
//! harness launch glue that task creation and the TUI need.

use anyhow::{Context, Result};

use crate::agent_model::{AgentAttachment, AgentRecord, AgentStatus};
use crate::config::Config;
use crate::task::Task;
use crate::tmux::Tmux;

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

pub fn ensure_task_tmux(config: &Config, task: &Task) -> Result<String> {
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
    let task_id = task.meta.task_id();
    match crate::use_cases::attached_agents_for_task(config, &task_id) {
        Ok(agents) => {
            for agent in agents {
                if let Err(e) =
                    crate::use_cases::link_agent_into_task_session(config, &agent, &task_id)
                {
                    tracing::warn!(
                        task_id = %task_id,
                        agent = %agent.meta.name,
                        error = %e,
                        "failed to backfill linked agent window into task tmux session"
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "failed to load attached agents for task tmux backfill"
            );
        }
    }

    supervisor_session(task)
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
