use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;
use crate::git::{self, Git};
use crate::repo_stats::RepoStats;
use crate::task::{RepoEntry, Task, TaskStatus};
use crate::tmux::Tmux;

/// Required external tools that must be on $PATH.
const REQUIRED_TOOLS: &[&str] = &["tmux", "git", "claude", "nvim", "lazygit", "gh", "direnv"];

/// Check that all required external tools are present on $PATH.
/// Returns a list of missing tool names (empty if all present).
pub fn check_dependencies() -> Vec<String> {
    let mut missing = Vec::new();
    for &tool in REQUIRED_TOOLS {
        let found = Command::new("which")
            .arg(tool)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !found {
            missing.push(tool.to_string());
        }
    }
    missing
}

/// Return an install hint for a missing tool.
pub fn install_hint(tool: &str) -> &'static str {
    match tool {
        "tmux" => "brew install tmux (macOS) / apt install tmux (Linux)",
        "git" => "brew install git (macOS) / apt install git (Linux)",
        "claude" => "npm install -g @anthropic-ai/claude-code",
        "nvim" => "brew install neovim (macOS) / apt install neovim (Linux)",
        "lazygit" => "brew install lazygit (macOS) / go install github.com/jesseduffield/lazygit@latest",
        "gh" => "brew install gh (macOS) / apt install gh (Linux)",
        "direnv" => "brew install direnv (macOS) / apt install direnv (Linux)",
        _ => "(see tool documentation)",
    }
}

/// How to handle the worktree when creating a task.
pub enum WorktreeSource {
    /// Create a brand-new worktree with a new branch.
    NewBranch,
    /// Create a worktree for an existing remote/local branch.
    ExistingBranch,
    /// Reuse an existing worktree directory.
    ExistingWorktree(PathBuf),
}

/// Create a new task: set up worktree, create task files, write TASK.md, increment repo stats.
/// Returns the created Task and its task_id.
///
/// This is the pure business logic behind `App::create_task_from_wizard()`.
/// It does NOT create tmux sessions or start flows — those are side effects
/// handled by the TUI caller.
pub fn create_task(
    config: &Config,
    repo_name: &str,
    branch_name: &str,
    description: &str,
    flow_name: &str,
    worktree_source: WorktreeSource,
    review_after: bool,
) -> Result<Task> {
    tracing::info!(
        repo = repo_name,
        branch = branch_name,
        flow = flow_name,
        review_after,
        "creating task"
    );
    // Initialize default files (flows, prompts, commands)
    config.init_default_files(false)?;

    // Set up or reuse worktree
    let worktree_path = match worktree_source {
        WorktreeSource::ExistingWorktree(path) => {
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::NewBranch => {
            let candidate = config.worktree_path(repo_name, branch_name);
            if candidate.exists() {
                tracing::info!(repo = repo_name, branch = branch_name, "worktree already exists, reusing");
                let _ = Git::direnv_allow(&candidate);
                candidate
            } else {
                let path = Git::create_worktree_quiet(config, repo_name, branch_name)?;
                let _ = Git::direnv_allow(&path);
                path
            }
        }
        WorktreeSource::ExistingBranch => {
            let candidate = config.worktree_path(repo_name, branch_name);
            if candidate.exists() {
                tracing::info!(repo = repo_name, branch = branch_name, "worktree already exists, reusing");
                let _ = Git::direnv_allow(&candidate);
                candidate
            } else {
                let path =
                    Git::create_worktree_for_existing_branch_quiet(config, repo_name, branch_name)?;
                let _ = Git::direnv_allow(&path);
                path
            }
        }
    };

    // Create task files
    let mut task = Task::create(
        config,
        repo_name,
        branch_name,
        description,
        flow_name,
        worktree_path,
    )?;

    // Ensure TASK.md is excluded from git tracking
    let _ = task.ensure_git_excludes_task();

    // Set review_after flag if requested
    if review_after {
        task.meta.review_after = true;
        task.save_meta()?;
    }

    // Increment repo usage stats
    let stats_path = config.repo_stats_path();
    let mut stats = RepoStats::load(&stats_path);
    stats.increment(repo_name);
    stats.save(&stats_path);

    Ok(task)
}

/// Create a multi-repo task: set up task dir and TASK.md, but no worktrees yet.
/// Repos will be populated by the `setup_repos` post-hook after the repo-inspector runs.
///
/// This is the pure business logic behind multi-repo task creation in the wizard.
/// It does NOT create tmux sessions or start flows — those are side effects
/// handled by the TUI caller.
pub fn create_multi_repo_task(
    config: &Config,
    name: &str,
    branch_name: &str,
    description: &str,
    flow_name: &str,
    parent_dir: PathBuf,
    review_after: bool,
) -> Result<Task> {
    tracing::info!(
        name = name,
        branch = branch_name,
        flow = flow_name,
        parent_dir = %parent_dir.display(),
        review_after,
        "creating multi-repo task"
    );

    // Initialize default files (flows, prompts, commands)
    config.init_default_files(false)?;

    // Create task files (no worktrees — repos not yet determined)
    let mut task = Task::create_multi(config, name, branch_name, description, flow_name, parent_dir)?;

    // Set review_after flag if requested
    if review_after {
        task.meta.review_after = true;
        task.save_meta()?;
    }

    Ok(task)
}

/// How to delete a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Remove worktree, delete branch, delete task dir.
    Everything,
    /// Remove TASK.md from worktree, delete task dir (keep worktree + branch).
    TaskOnly,
}

/// Delete a task according to the specified mode.
///
/// This is the pure business logic behind `App::delete_task()`.
/// It does NOT kill tmux sessions — that's a side effect handled by the caller.
pub fn delete_task(config: &Config, task: Task, mode: DeleteMode) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), mode = ?mode, "deleting task");

    match mode {
        DeleteMode::Everything => {
            // Remove worktrees and branches for all repos
            for repo in &task.meta.repos {
                let repo_path = config.repo_path(&repo.repo_name);
                let _ = Git::remove_worktree(&repo_path, &repo.worktree_path);
                let _ = Git::delete_branch(&repo_path, &task.meta.branch_name);
            }
        }
        DeleteMode::TaskOnly => {
            // TASK.md is in the task dir now, no worktree cleanup needed
        }
    }

    // Delete task directory (includes TASK.md)
    task.delete(config)?;
    Ok(())
}

/// Stop a running task: set status to Stopped and clear current_agent.
///
/// This is the pure business logic behind `App::stop_task()`.
/// It does NOT send Ctrl+C to tmux — that's a side effect handled by the caller.
pub fn stop_task(task: &mut Task) -> Result<()> {
    if task.meta.status == TaskStatus::Stopped {
        return Ok(());
    }
    tracing::info!(task_id = %task.meta.task_id(), old_status = %task.meta.status, new_status = "stopped", "stopping task");
    task.update_status(TaskStatus::Stopped)?;
    task.meta.current_agent = None;
    task.save_meta()?;
    Ok(())
}

/// Put a stopped task on hold.
///
/// This is the pure business logic behind `App::toggle_hold()`.
/// Only transitions from Stopped → OnHold.
pub fn put_on_hold(task: &mut Task) -> Result<()> {
    if task.meta.status != TaskStatus::Stopped {
        return Ok(());
    }
    tracing::info!(task_id = %task.meta.task_id(), old_status = "stopped", new_status = "on_hold", "putting task on hold");
    task.update_status(TaskStatus::OnHold)?;
    Ok(())
}

/// Resume a task from on-hold back to stopped.
///
/// This is the pure business logic behind `App::toggle_hold()`.
/// Only transitions from OnHold → Stopped.
pub fn resume_from_hold(task: &mut Task) -> Result<()> {
    if task.meta.status != TaskStatus::OnHold {
        return Ok(());
    }
    tracing::info!(task_id = %task.meta.task_id(), old_status = "on_hold", new_status = "stopped", "resuming task from hold");
    task.update_status(TaskStatus::Stopped)?;
    Ok(())
}

/// Resume a task after the user has answered questions: set status back to Running.
///
/// This is the pure business logic behind `App::resume_after_answering()`.
/// It does NOT create tmux sessions or dispatch flows — those are side effects.
pub fn resume_after_answering(task: &mut Task) -> Result<()> {
    if task.meta.status != TaskStatus::InputNeeded {
        return Ok(());
    }
    tracing::info!(task_id = %task.meta.task_id(), old_status = "input_needed", new_status = "running", "resuming task after answering");
    task.update_status(TaskStatus::Running)?;
    Ok(())
}

/// Queue feedback on a running task.
///
/// Extracts the "running" branch of `App::submit_feedback()`.
pub fn queue_feedback(task: &Task, feedback: &str) -> Result<usize> {
    tracing::info!(task_id = %task.meta.task_id(), "queuing feedback");
    task.append_feedback_to_log(feedback)?;
    task.queue_feedback(feedback)?;
    Ok(task.queued_feedback_count())
}

/// Write immediate feedback for a stopped task.
///
/// Extracts the "stopped" branch of `App::submit_feedback()`.
/// Does NOT run `agman continue` — that's a side effect handled by the caller.
pub fn write_immediate_feedback(task: &Task, feedback: &str) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "writing immediate feedback");
    task.write_feedback(feedback)
}

/// Delete a single queued feedback item by index.
pub fn delete_queued_feedback(task: &Task, index: usize) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), index, "deleting queued feedback item");
    task.remove_feedback_queue_item(index)
}

/// Clear all queued feedback.
pub fn clear_all_queued_feedback(task: &Task) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "clearing all queued feedback");
    task.clear_feedback_queue()
}

/// List all tasks, sorted by status (running > input_needed > stopped) then by updated_at desc.
pub fn list_tasks(config: &Config) -> Vec<Task> {
    Task::list_all(config)
}

/// Save notes for a task.
pub fn save_notes(task: &Task, notes: &str) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "saving notes");
    task.write_notes(notes)
}

/// Save TASK.md content for a task.
pub fn save_task_file(task: &Task, content: &str) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "saving task file");
    task.write_task(content)
}

/// List all stored commands from the commands directory.
pub fn list_commands(config: &Config) -> Result<Vec<crate::command::StoredCommand>> {
    crate::command::StoredCommand::list_all(&config.commands_dir)
}

/// Restart a task from a specific flow step: set flow_step and status to Running.
///
/// This is the pure business logic behind `App::execute_restart_wizard()`.
/// It does NOT create tmux sessions or dispatch flow-run — those are side effects.
pub fn restart_task(task: &mut Task, step_index: usize) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), step = step_index, old_status = %task.meta.status, new_status = "running", "restarting task");
    task.meta.flow_step = step_index;
    task.update_status(TaskStatus::Running)?;
    Ok(())
}

/// Pop the first queued feedback item and write it as immediate feedback.
///
/// This is the pure business logic behind `App::process_stranded_feedback()`.
/// It does NOT run `agman continue` — that's a side effect handled by the caller.
/// Returns the feedback string if one was popped, or None if the queue was empty.
pub fn pop_and_apply_feedback(task: &Task) -> Result<Option<String>> {
    tracing::info!(task_id = %task.meta.task_id(), "popping and applying queued feedback");
    match task.pop_feedback_queue()? {
        Some(feedback) => {
            task.write_feedback(&feedback)?;
            Ok(Some(feedback))
        }
        None => Ok(None),
    }
}

/// Action to take for a task after polling its linked PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrPollAction {
    /// No action needed.
    None,
    /// PR was merged — delete the task.
    DeleteMerged,
    /// New review detected — run the address-review command.
    AddressReview { new_count: u64 },
}

/// Pure decision function: given PR state data, determine what action to take.
///
/// - If the PR is merged, always delete.
/// - If `last_review_count` is `None`, this is the first poll — return `None` (just seed the count).
/// - If the current count exceeds the stored count, a new review arrived.
/// - Otherwise, no action.
pub fn determine_pr_poll_action(
    _status: TaskStatus,
    pr_merged: bool,
    current_review_count: u64,
    last_review_count: Option<u64>,
) -> PrPollAction {
    if pr_merged {
        return PrPollAction::DeleteMerged;
    }
    match last_review_count {
        Some(prev) if current_review_count > prev => PrPollAction::AddressReview {
            new_count: current_review_count,
        },
        None => PrPollAction::None, // first poll, just seed
        _ => PrPollAction::None,
    }
}

/// Set the `review_addressed` flag on a task and persist to disk.
pub fn set_review_addressed(task: &mut Task, addressed: bool) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), addressed, "setting review_addressed flag");
    task.meta.review_addressed = addressed;
    task.save_meta()
}

/// Update the `last_review_count` on a task and persist to disk.
pub fn update_last_review_count(task: &mut Task, count: u64) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), count, "updating last review count");
    task.meta.last_review_count = Some(count);
    task.save_meta()
}

/// Set the linked PR for a task by constructing the URL from the worktree's origin remote.
pub fn set_linked_pr(task: &mut Task, pr_number: u64, worktree_path: &PathBuf, owned: bool) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), pr_number, owned, "setting linked PR");
    let remote_url = Git::get_remote_url(worktree_path)?;
    let (owner, repo) = git::parse_github_owner_repo(&remote_url)
        .ok_or_else(|| anyhow::anyhow!("Not a GitHub remote: {}", remote_url))?;
    let url = format!("https://github.com/{}/{}/pull/{}", owner, repo, pr_number);
    task.set_linked_pr(pr_number, url, owned)
}

/// Clear the linked PR and reset stale polling state.
pub fn clear_linked_pr(task: &mut Task) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "clearing linked PR");
    task.meta.linked_pr = None;
    task.meta.last_review_count = None;
    task.meta.review_addressed = false;
    task.meta.updated_at = chrono::Utc::now();
    task.save_meta()
}

/// Create a setup-only task: set up worktree, create task files, but do NOT start any flow.
/// The task will have `Stopped` status so the user can attach to the tmux session and explore.
///
/// This is the pure business logic behind `App::create_setup_only_task_from_wizard()`.
/// It does NOT create tmux sessions — those are side effects handled by the TUI caller.
pub fn create_setup_only_task(
    config: &Config,
    repo_name: &str,
    branch_name: &str,
    worktree_source: WorktreeSource,
) -> Result<Task> {
    tracing::info!(
        repo = repo_name,
        branch = branch_name,
        "creating setup-only task"
    );
    // Initialize default files (flows, prompts, commands)
    config.init_default_files(false)?;

    // Set up or reuse worktree
    let worktree_path = match worktree_source {
        WorktreeSource::ExistingWorktree(path) => {
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::NewBranch => {
            let path = Git::create_worktree_quiet(config, repo_name, branch_name)?;
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::ExistingBranch => {
            let path =
                Git::create_worktree_for_existing_branch_quiet(config, repo_name, branch_name)?;
            let _ = Git::direnv_allow(&path);
            path
        }
    };

    // Create task files
    let mut task = Task::create(
        config,
        repo_name,
        branch_name,
        "",
        "none",
        worktree_path,
    )?;

    // Override status to Stopped (Task::create sets Running by default)
    task.update_status(TaskStatus::Stopped)?;

    // Write a minimal TASK.md (empty goal, ready for user to fill via feedback)
    task.write_task("# Goal\n\n# Plan\n")?;

    // Ensure TASK.md is excluded from git tracking
    let _ = task.ensure_git_excludes_task();

    // Increment repo usage stats
    let stats_path = config.repo_stats_path();
    let mut stats = RepoStats::load(&stats_path);
    stats.increment(repo_name);
    stats.save(&stats_path);

    tracing::info!(task_id = %task.meta.task_id(), "setup-only task created");

    Ok(task)
}

/// Create a review task for an existing branch.
///
/// Similar to `create_task` but uses a review-specific description
/// and doesn't require a user-provided description.
/// Does NOT create tmux sessions or run commands — those are side effects.
pub fn create_review_task(
    config: &Config,
    repo_name: &str,
    branch_name: &str,
    worktree_source: WorktreeSource,
) -> Result<Task> {
    tracing::info!(repo = repo_name, branch = branch_name, "creating review task");
    let description = format!("Review branch {}", branch_name);
    create_task(
        config,
        repo_name,
        branch_name,
        &description,
        "new",
        worktree_source,
        false,
    )
}

/// Parse repo names from the `# Repos` section in TASK.md content.
///
/// Expects lines matching `- <name>: <rationale>` under the `# Repos` heading.
/// Returns just the repo names.
pub fn parse_repos_from_task_md(content: &str) -> Vec<String> {
    let mut repos = Vec::new();
    let mut in_repos_section = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == "# Repos" {
            in_repos_section = true;
            continue;
        }

        // A new top-level heading ends the Repos section
        if in_repos_section && trimmed.starts_with("# ") {
            break;
        }

        if in_repos_section {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                // Extract repo name before the colon
                let name = if let Some(colon_pos) = rest.find(':') {
                    rest[..colon_pos].trim()
                } else {
                    rest.trim()
                };
                if !name.is_empty() {
                    repos.push(name.to_string());
                }
            }
        }
    }

    repos
}

/// Post-hook: read `# Repos` from TASK.md, create worktrees + tmux sessions,
/// and populate `task.meta.repos`.
///
/// Called after the repo-inspector agent finishes (via the `setup_repos` post-hook).
pub fn setup_repos_from_task_md(config: &Config, task: &mut Task) -> Result<()> {
    let task_content = task
        .read_task()
        .context("Failed to read TASK.md for repo setup")?;
    let repo_names = parse_repos_from_task_md(&task_content);

    if repo_names.is_empty() {
        anyhow::bail!(
            "No repos found in TASK.md # Repos section for task '{}'",
            task.meta.task_id()
        );
    }

    tracing::info!(
        task_id = %task.meta.task_id(),
        repos = ?repo_names,
        "setting up repos from TASK.md"
    );

    let mut entries = Vec::new();

    for repo_name in &repo_names {
        // Check if worktree already exists (idempotent on retry after partial failure)
        let candidate = config.worktree_path(repo_name, &task.meta.branch_name);
        let worktree_path = if candidate.exists() {
            tracing::info!(repo = repo_name, branch = %task.meta.branch_name, "worktree already exists, reusing");
            let _ = Git::direnv_allow(&candidate);
            candidate
        } else {
            let path = Git::create_worktree_quiet(config, repo_name, &task.meta.branch_name)
                .with_context(|| format!("Failed to create worktree for repo '{}'", repo_name))?;
            let _ = Git::direnv_allow(&path);
            path
        };

        // Create tmux session (best-effort — tmux may not be available in tests)
        let tmux_session = Config::tmux_session_name(repo_name, &task.meta.branch_name);
        if !Tmux::session_exists(&tmux_session) {
            if let Err(e) = Tmux::create_session_with_windows(&tmux_session, &worktree_path) {
                tracing::warn!(repo = repo_name, error = %e, "failed to create tmux session (non-fatal)");
            } else {
                let _ = Tmux::add_review_window(&tmux_session, &worktree_path);
            }
        }

        // Ensure REVIEW.md is excluded from git tracking
        let _ = task.ensure_git_excludes_for_worktree(&worktree_path);

        entries.push(RepoEntry {
            repo_name: repo_name.clone(),
            worktree_path,
            tmux_session,
        });

        // Save incrementally so partial progress is preserved on failure
        task.meta.repos = entries.clone();
        task.save_meta()?;
    }

    // Final save (entries == task.meta.repos at this point, but be explicit)
    task.meta.repos = entries;
    task.save_meta()?;

    tracing::info!(
        task_id = %task.meta.task_id(),
        repo_count = repo_names.len(),
        "repos setup complete"
    );

    Ok(())
}
