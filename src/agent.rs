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
    /// passed to claude via `--system-prompt-file`.
    ///
    /// The shape of the system prompt depends on `command_mode`:
    ///
    /// - **Regular flows** (coder/checker/refiner — `command_mode = false`):
    ///   the agent's `prompt_template` IS the identity ("You are a coder /
    ///   checker / refiner agent. Read TASK.md, …") and TASK.md is the dynamic
    ///   work directive delivered through the inbox. The system prompt embeds
    ///   the prompt template plus shared boilerplate (work directives, skills,
    ///   task-dir paths, self-improve, sentinel rules).
    /// - **Stored commands** (pr-creator / pr-merge-agent / push-rebaser etc.
    ///   — `command_mode = true`): the prompt template IS the action
    ///   ("Create a draft PR: 1. run `gh pr view` 2. …"). System prompts
    ///   don't trigger work — claude needs a real user message — so the
    ///   prompt template moves to the inbox message via
    ///   [`build_inbox_message`](Self::build_inbox_message). The system prompt
    ///   keeps only the shared boilerplate, which tells the agent it will
    ///   receive its work assignment in the inbox.
    ///
    /// The dynamic per-launch payload (TASK.md, feedback, git diff, recent
    /// commits) is never embedded in the system prompt — it lives in
    /// [`build_inbox_message`](Self::build_inbox_message) and is delivered to
    /// the agent via the task inbox once claude is ready.
    pub fn build_system_prompt(&self, task: &Task, command_mode: bool) -> Result<String> {
        let mut prompt = String::new();

        // Regular flows embed the prompt template as identity-as-instruction.
        // Stored commands intentionally omit it — the action moves to the
        // inbox message because system prompts don't trigger claude into
        // doing work.
        if !command_mode {
            prompt.push_str(&self.prompt_template);
            prompt.push_str("\n\n---\n\n");
        }

        // Work-directives preamble: tell the agent to wait for the inbox
        // message before doing anything. The system prompt is identity only.
        // The sender tag is the project (PM) name when available; falls back
        // to "supervisor" for unassigned tasks.
        let sender = task.meta.project.as_deref().unwrap_or("supervisor");
        prompt.push_str("## Work Directives\n");
        if command_mode {
            prompt.push_str(&format!(
                "You will receive your work assignment as a tmux message tagged\n[Message from {sender}]:. The message contains the action you must perform plus relevant task context. Begin work when that message arrives. Do not act before then — the system prompt only tells you how to finish, not what to do.\n"
            ));
        } else {
            prompt.push_str(&format!(
                "You will receive your work assignment as a tmux message tagged\n[Message from {sender}]: containing the current TASK.md, any feedback,\nand current git context. Begin work when that message arrives. Do not act\nbefore then — the system prompt only describes who you are and how to\nfinish, not what to do.\n"
            ));
        }

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
        prompt.push_str("**Before completing**, check if the repository has a self-improvement skill (commonly named \"self-improve\" or similar) in `.claude/skills/` or `.claude/commands/`. If one exists, run it. This helps keep project documentation and conventions up to date.\n");

        // Sentinel footer: the supervisor detects completion purely by file
        // existence. The agent's LAST action is to `touch` ONE of three
        // sentinel files. No content is read from them — the supervisor only
        // checks whether they exist. Do NOT print "magic strings" — they are
        // not used for detection (they are unreliable in interactive mode).
        let dir = task.dir.display();
        prompt.push_str("\n\n---\n\n");
        prompt.push_str("# Supervisor Sentinel (REQUIRED — last action)\n");
        prompt.push_str(
            "When you are done, signal the outcome by creating ONE of the following sentinel files in the task directory. The supervisor polls for their existence — touching the file is the signal. Do NOT write any content into the file; only its presence matters.\n\n",
        );
        prompt.push_str(&format!(
            "- Finished this step, hand off to the next agent in the flow:\n  ```\n  touch {dir}/.agent-done\n  ```\n- Entire task is complete, stop the flow:\n  ```\n  touch {dir}/.task-complete\n  ```\n- Need user input to continue, pause the flow:\n  ```\n  touch {dir}/.input-needed\n  ```\n"
        ));
        prompt.push_str("\nThis MUST be your very last action. The supervisor will not advance until one of these files exists. Create exactly one — they are mutually exclusive.\n");

        Ok(prompt)
    }

    /// Build the **inbox message** for this launch — the dynamic per-launch
    /// work payload delivered to the agent's tmux session via the task inbox.
    ///
    /// Shape depends on `command_mode`:
    ///
    /// - **Regular flows** (`command_mode = false`): TASK.md is the work
    ///   directive. The message contains TASK.md, any pending FEEDBACK.md,
    ///   the current git diff, and recent commits.
    /// - **Stored commands** (`command_mode = true`): the agent's prompt
    ///   template IS the work directive (e.g. "Create a draft PR: …").
    ///   The message starts with the prompt template under `# Action`,
    ///   followed by TASK.md as `# Task context (for reference)`, plus the
    ///   current git diff and recent commits.
    ///
    /// This is paired with [`build_system_prompt`](Self::build_system_prompt)
    /// — the system prompt is "who you are and how to finish", and the inbox
    /// message is "what to do right now".
    pub fn build_inbox_message(&self, task: &Task, command_mode: bool) -> Result<String> {
        if command_mode {
            return self.build_command_inbox_message(task);
        }

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

    /// Inbox message for stored-command agents. The agent's prompt template
    /// is the action directive; TASK.md and git context are reference only.
    fn build_command_inbox_message(&self, task: &Task) -> Result<String> {
        let task_content = task.read_task()?;

        let mut msg = String::new();
        // The action instructions ARE the work directive for command flows —
        // they live in the prompt template (e.g. PR_CREATOR_PROMPT) and would
        // never trigger work if left in the system prompt. Putting them at
        // the top of the inbox message gets claude to actually act.
        msg.push_str("# Action\n");
        msg.push_str(&self.prompt_template);
        msg.push_str("\n\n");

        // TASK.md is reference context, not a directive. The action above
        // tells the agent what to do; TASK.md just describes the state of
        // the task it's operating against.
        msg.push_str("# Task context (for reference)\n");
        msg.push_str(&task_content);
        msg.push_str("\n\n");

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
