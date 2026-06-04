use anyhow::{Context, Result};

use crate::config::Config;
use crate::harness::Harness;
use crate::task::Task;

pub struct Agent {
    pub name: String,
    pub prompt_template: String,
}

impl Agent {
    pub fn load(config: &Config, name: &str) -> Result<Self> {
        let prompt_path = config.prompt_path(name);
        let prompt_template = std::fs::read_to_string(&prompt_path)
            .with_context(|| format!("Failed to load agent prompt: {}", name))?;

        Ok(Self {
            name: name.to_string(),
            prompt_template,
        })
    }

    /// Build the static system prompt for this agent. Dynamic task context is
    /// delivered later as an inbox message so the long-lived agent can treat
    /// inbox traffic as the source of work and status coordination.
    pub fn build_system_prompt(
        &self,
        task: &Task,
        _command_mode: bool,
        harness: &dyn Harness,
    ) -> Result<String> {
        let mut prompt = String::new();

        prompt.push_str(&self.prompt_template);
        prompt.push_str("\n\n---\n\n");

        // The sender tag is the project (PM) name when available; falls back
        // to "pm" for unassigned tasks.
        let sender = task.meta.project.as_deref().unwrap_or("chief-of-staff");
        prompt.push_str("## Work Directives\n");
        prompt.push_str(&format!(
            "You will receive work, blockers, and follow-ups as tmux messages tagged\n[Message from {sender}]:. Treat inbox messages as the coordination channel with the PM and other agents. Report progress, blockers, and completion back through agman send-message.\n"
        ));

        // Append skill-awareness footer so the agent sees it early. The
        // wording is harness-specific (claude knows about `.claude/skills/`,
        // codex has no equivalent today). Skip the section entirely when
        // the harness's hint is empty.
        let skill_hint = harness.skill_hint();
        if !skill_hint.is_empty() {
            prompt.push_str("\n\n---\n\n");
            prompt.push_str("# Skills\n");
            prompt.push_str(skill_hint);
            prompt.push('\n');
        }

        // Add task directory info for task-attached agents.
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Task Directory\n");
        prompt.push_str(&format!("Task state directory: {}\n", task.dir.display()));

        // For multi-repo tasks, list all repos and their worktree paths
        if task.meta.is_multi_repo() && !task.meta.repos.is_empty() {
            prompt.push_str("\n# Repo Worktrees\n");
            for repo in &task.meta.repos {
                prompt.push_str(&format!(
                    "- {}: {}\n",
                    repo.repo_name,
                    repo.worktree_path.display()
                ));
            }
        }

        // Append self-improve footer to all prompts (claude only — codex's
        // skill hint is empty so we skip this for codex).
        if !skill_hint.is_empty() {
            prompt.push_str("\n\n---\n\n");
            prompt.push_str("# Self-Improvement\n");
            prompt.push_str("**Before completing**, check if the repository has a self-improvement skill (commonly named \"self-improve\" or similar) in `.claude/skills/` or `.claude/commands/`. If one exists, run it. This helps keep project documentation and conventions up to date.\n");
        }

        Ok(prompt)
    }

    /// Build lightweight task context for a task-attached long-lived agent.
    /// A nonblank first prompt is delivered as an inbox message at task creation.
    pub fn build_inbox_message(&self, task: &Task, _command_mode: bool) -> Result<String> {
        let mut msg = String::new();

        if let Ok(diff) = task.get_git_diff() {
            if !diff.is_empty() {
                msg.push_str("# Current Git Diff\n");
                msg.push_str("```diff\n");
                if diff.len() > 10000 {
                    msg.push_str(&diff[..10000]);
                    msg.push_str("\n... (truncated)\n");
                } else {
                    msg.push_str(&diff);
                }
                msg.push_str("```\n\n");
            }
        }

        if let Ok(log) = task.get_git_log_summary() {
            if !log.is_empty() {
                msg.push_str("# Recent Commits\n");
                msg.push_str("```\n");
                msg.push_str(&log);
                msg.push_str("```\n");
            }
        }

        Ok(msg)
    }
}
