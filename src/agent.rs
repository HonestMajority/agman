use anyhow::{Context, Result};

use crate::config::Config;
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

    /// Build the **system prompt** for this agent — the static identity payload
    /// passed to claude via `--system-prompt-file`. Contains: prompt template,
    /// skills footer, task-dir paths, multi-repo paths, self-improve footer,
    /// and the supervisor sentinel directive (.agent-done path).
    ///
    /// Does **not** contain the dynamic per-launch payload (TASK.md content,
    /// feedback, git diff, git log) — that lives in
    /// [`build_inbox_message`](Self::build_inbox_message) and is delivered to
    /// the agent via the task inbox once claude is ready.
    pub fn build_system_prompt(&self, task: &Task) -> Result<String> {
        let mut prompt = self.prompt_template.clone();

        // Work-directives preamble: tell the agent to wait for the inbox
        // message before doing anything. The system prompt is identity only.
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("## Work Directives\n");
        prompt.push_str("You will receive your work assignment as a tmux message tagged\n");
        prompt.push_str("[Message from supervisor]: containing the current TASK.md, any feedback,\n");
        prompt.push_str("and current git context. Begin work when that message arrives. Do not act\n");
        prompt.push_str("before then — the system prompt only describes who you are and how to\n");
        prompt.push_str("finish, not what to do.\n");

        // Append skill-awareness footer so the agent sees it early
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Skills\n");
        prompt.push_str("Before starting, check if the repository has Claude Code skills defined in `.claude/skills/` or `.claude/commands/`. If any are relevant to your task, use them.\n");

        // Add task directory info so agents know where TASK.md lives
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Task Directory\n");
        prompt.push_str(&format!(
            "TASK.md is located at: {}/TASK.md\n",
            task.dir.display()
        ));

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

        // Append self-improve footer to all prompts
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Self-Improvement\n");
        prompt.push_str("**Before outputting your final stop condition**, check if the repository has a self-improvement skill (commonly named \"self-improve\" or similar) in `.claude/skills/` or `.claude/commands/`. If one exists, run it. This helps keep project documentation and conventions up to date.\n");

        // Sentinel footer: the supervisor watches for this file to detect when
        // the agent is done. Writing it is the agent's LAST action, after
        // printing the magic string.
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Supervisor Sentinel (REQUIRED — last action)\n");
        prompt.push_str(&format!(
            "After printing your stop condition (`AGENT_DONE`, `TASK_COMPLETE`, or `INPUT_NEEDED`), you MUST, as your very last action, write that condition to `{}/.agent-done`. Example:\n\n",
            task.dir.display()
        ));
        prompt.push_str("```\n");
        prompt.push_str(&format!(
            "echo AGENT_DONE > {}/.agent-done\n",
            task.dir.display()
        ));
        prompt.push_str("```\n\n");
        prompt.push_str("Do this EVEN IF you already said you are done. The supervisor does not proceed until this file exists. Do not write anything else to the file — just the single magic string.\n");

        Ok(prompt)
    }

    /// Build the **inbox message** for this launch — the dynamic per-launch
    /// work payload delivered to the agent's tmux session via the task inbox.
    /// Contains: the current TASK.md, any pending FEEDBACK.md, the current
    /// git diff, and the recent commit log.
    ///
    /// This is paired with [`build_system_prompt`](Self::build_system_prompt)
    /// — the system prompt is "who you are and how to finish", and the inbox
    /// message is "what to do right now".
    pub fn build_inbox_message(&self, task: &Task) -> Result<String> {
        let task_content = task.read_task()?;
        let feedback = task.read_feedback()?;

        let mut msg = String::new();
        msg.push_str("# Current Task\n");
        msg.push_str(&task_content);
        msg.push_str("\n\n");

        // Include git context for refiner and checker agents (and whenever
        // there's queued feedback)
        let needs_git_context =
            !feedback.is_empty() || self.name == "checker" || self.name == "refiner";

        if needs_git_context {
            // Include feedback if present
            if !feedback.is_empty() {
                msg.push_str("# Follow-up Feedback\n");
                msg.push_str(&feedback);
                msg.push_str("\n\n");
            }

            // Include git diff for context
            if let Ok(diff) = task.get_git_diff() {
                if !diff.is_empty() {
                    msg.push_str("# Current Git Diff\n");
                    msg.push_str("```diff\n");
                    // Truncate if too long
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
        }

        Ok(msg)
    }
}
