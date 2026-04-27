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

    pub fn build_prompt(&self, task: &Task) -> Result<String> {
        let task_content = task.read_task()?;
        let feedback = task.read_feedback()?;

        let mut prompt = self.prompt_template.clone();

        // Append skill-awareness footer before task content so the agent sees it early
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

        prompt.push_str("\n---\n\n");
        prompt.push_str("# Current Task\n");
        prompt.push_str(&task_content);
        prompt.push_str("\n\n");

        // Include git context for refiner and checker agents
        let needs_git_context =
            !feedback.is_empty() || self.name == "checker" || self.name == "refiner";

        if needs_git_context {
            // Include feedback if present
            if !feedback.is_empty() {
                prompt.push_str("# Follow-up Feedback\n");
                prompt.push_str(&feedback);
                prompt.push_str("\n\n");
            }

            // Include git diff for context
            if let Ok(diff) = task.get_git_diff() {
                if !diff.is_empty() {
                    prompt.push_str("# Current Git Diff\n");
                    prompt.push_str("```diff\n");
                    // Truncate if too long
                    if diff.len() > 10000 {
                        prompt.push_str(&diff[..10000]);
                        prompt.push_str("\n... (truncated)\n");
                    } else {
                        prompt.push_str(&diff);
                    }
                    prompt.push_str("```\n\n");
                }
            }

            if let Ok(log) = task.get_git_log_summary() {
                if !log.is_empty() {
                    prompt.push_str("# Recent Commits\n");
                    prompt.push_str("```\n");
                    prompt.push_str(&log);
                    prompt.push_str("```\n");
                }
            }
        }

        // Append self-improve footer to all prompts
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Self-Improvement\n");
        prompt.push_str("**Before outputting your final stop condition**, check if the repository has a self-improvement skill (commonly named \"self-improve\" or similar) in `.claude/skills/` or `.claude/commands/`. If one exists, run it. This helps keep project documentation and conventions up to date.\n");

        // Sentinel footer: the supervisor watches for this file to detect when
        // the agent is done. Writing it is the agent's LAST action, after
        // printing the magic string. Printing `<MAGIC>:<session_id>` makes the
        // stop condition visible in the tmux scrollback for pane-scanning too.
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
}
