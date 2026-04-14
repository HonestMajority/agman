use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::Config;
use crate::flow::{Flow, FlowStep};
use crate::git::{self, Git};
use crate::inbox;
use crate::project::Project;
use crate::repo_stats::RepoStats;
use crate::researcher::{Researcher, ResearcherStatus};
use crate::task::{QueueItem, RepoEntry, Task, TaskStatus};
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

/// Copy `.env` from the main repo to a new worktree if it exists.
///
/// Best-effort: logs a warning on failure, never fails task creation.
pub fn copy_repo_files_to_worktree(
    config: &Config,
    repo_name: &str,
    worktree_path: &Path,
    parent_dir: Option<&Path>,
) -> Result<()> {
    let repo_root = config.repo_path_for(parent_dir, repo_name);
    let src = repo_root.join(".env");

    if !src.exists() {
        return Ok(());
    }

    let dst = worktree_path.join(".env");
    if dst.exists() {
        tracing::debug!(repo = repo_name, "worktree already has .env, skipping copy");
        return Ok(());
    }

    match std::fs::copy(&src, &dst) {
        Ok(_) => {
            tracing::info!(repo = repo_name, "copied .env to worktree");
        }
        Err(e) => {
            tracing::warn!(repo = repo_name, error = %e, "failed to copy .env to worktree");
        }
    }

    Ok(())
}

/// How to handle the worktree when creating a task.
pub enum WorktreeSource {
    /// Create a brand-new worktree with a new branch.
    /// If `base_branch` is `Some`, use it as the base ref; otherwise auto-detect.
    NewBranch { base_branch: Option<String> },
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
    parent_dir: Option<PathBuf>,
    project: Option<String>,
) -> Result<Task> {
    tracing::info!(
        repo = repo_name,
        branch = branch_name,
        flow = flow_name,
        review_after,
        "creating task"
    );
    let parent_dir_ref = parent_dir.as_deref();

    // Initialize default files (flows, prompts, commands)
    config.init_default_files(false)?;

    // Set up or reuse worktree
    let worktree_path = match worktree_source {
        WorktreeSource::ExistingWorktree(path) => {
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::NewBranch { base_branch } => {
            let candidate = config.worktree_path_for(parent_dir_ref, repo_name, branch_name);
            if candidate.exists() {
                tracing::info!(repo = repo_name, branch = branch_name, "worktree already exists, reusing");
                let _ = Git::direnv_allow(&candidate);
                candidate
            } else {
                let path = Git::create_worktree_quiet(config, repo_name, branch_name, base_branch.as_deref(), parent_dir_ref)?;
                let _ = Git::direnv_allow(&path);
                path
            }
        }
        WorktreeSource::ExistingBranch => {
            let candidate = config.worktree_path_for(parent_dir_ref, repo_name, branch_name);
            if candidate.exists() {
                tracing::info!(repo = repo_name, branch = branch_name, "worktree already exists, reusing");
                let _ = Git::direnv_allow(&candidate);
                candidate
            } else {
                let path =
                    Git::create_worktree_for_existing_branch_quiet(config, repo_name, branch_name, parent_dir_ref)?;
                let _ = Git::direnv_allow(&path);
                path
            }
        }
    };

    // Copy configured files (e.g. .env) from main repo to worktree (best-effort)
    if let Err(e) = copy_repo_files_to_worktree(config, repo_name, &worktree_path, parent_dir_ref) {
        tracing::warn!(repo = repo_name, branch = branch_name, error = %e, "failed to copy repo files to worktree");
    }

    // Create task files
    let mut task = Task::create(
        config,
        repo_name,
        branch_name,
        description,
        flow_name,
        worktree_path,
    )?;

    // Store parent_dir if repo is outside repos_dir
    if parent_dir.is_some() {
        task.meta.parent_dir = parent_dir;
    }

    // Set review_after flag if requested
    if review_after {
        task.meta.review_after = true;
    }

    // Assign to project if specified
    if project.is_some() {
        task.meta.project = project;
    }

    // Ensure TASK.md is excluded from git tracking
    let _ = task.ensure_git_excludes_task();

    // Save if any optional fields were set after creation
    if task.meta.parent_dir.is_some() || task.meta.review_after || task.meta.project.is_some() {
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
    project: Option<String>,
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
    }

    // Assign to project if specified
    if project.is_some() {
        task.meta.project = project;
    }

    // Save if any optional fields were set after creation
    if task.meta.review_after || task.meta.project.is_some() {
        task.save_meta()?;
    }

    Ok(task)
}

/// Archive a task: remove worktrees, set archived_at and saved, save meta.
///
/// Branches are preserved so the user can revisit them later. They are cleaned
/// up when the task is permanently deleted (see `permanently_delete_archived_task`).
///
/// This is the pure business logic behind archiving from the task list.
/// It does NOT kill tmux sessions — that's a side effect handled by the caller.
/// It does NOT remove the task directory — the directory is kept as the archive.
pub fn archive_task(config: &Config, task: &mut Task, saved: bool) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), saved, "archiving task");

    // Remove worktrees (branches are kept for later reference)
    let parent_dir = task.meta.parent_dir.as_deref();
    for repo in &task.meta.repos {
        let repo_path = config.repo_path_for(parent_dir, &repo.repo_name);
        let _ = Git::remove_worktree(&repo_path, &repo.worktree_path);
    }

    // Mark as archived
    task.meta.archived_at = Some(chrono::Utc::now());
    task.meta.saved = saved;
    task.save_meta()?;

    Ok(())
}

/// Permanently delete an archived task by deleting its git branches and removing
/// its directory from disk.
///
/// Used from the archive view and by `purge_old_archives`. Branch deletion is
/// best-effort — branches may have already been manually deleted.
pub fn permanently_delete_archived_task(config: &Config, task: Task) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "permanently deleting archived task");

    // Delete branches for all repos (best-effort)
    let parent_dir = task.meta.parent_dir.as_deref();
    for repo in &task.meta.repos {
        let repo_path = config.repo_path_for(parent_dir, &repo.repo_name);
        let _ = Git::delete_branch(&repo_path, &task.meta.branch_name);
    }

    task.delete(config)?;
    Ok(())
}

/// Fully delete a task: remove worktrees, delete branches, and remove the task
/// directory. This is the "nuclear option" — everything is gone immediately.
///
/// Like `archive_task`, this does NOT kill tmux sessions — the caller handles that.
pub fn fully_delete_task(config: &Config, task: Task) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "fully deleting task");

    // Remove worktrees (best-effort)
    let parent_dir = task.meta.parent_dir.as_deref();
    for repo in &task.meta.repos {
        let repo_path = config.repo_path_for(parent_dir, &repo.repo_name);
        let _ = Git::remove_worktree(&repo_path, &repo.worktree_path);
    }

    // Delete branches (best-effort)
    for repo in &task.meta.repos {
        let repo_path = config.repo_path_for(parent_dir, &repo.repo_name);
        let _ = Git::delete_branch(&repo_path, &task.meta.branch_name);
    }

    task.delete(config)?;
    Ok(())
}

/// Toggle the saved flag on an archived task.
pub fn toggle_archive_saved(_config: &Config, task: &mut Task) -> Result<()> {
    let new_saved = !task.meta.saved;
    tracing::info!(task_id = %task.meta.task_id(), saved = new_saved, "toggling archive saved");
    let old_saved = task.meta.saved;
    task.meta.saved = new_saved;
    if let Err(e) = task.save_meta() {
        task.meta.saved = old_saved;
        return Err(e);
    }
    Ok(())
}

/// Purge expired archived tasks that are not saved.
///
/// Delegates to `permanently_delete_archived_task` so that branch cleanup is
/// centralized in one place. Returns the count of purged tasks.
pub fn purge_old_archives(config: &Config) -> Result<usize> {
    let retention_days = load_archive_retention(config);
    let archived = Task::list_archived(config);
    let mut purged = 0;

    for task in archived {
        if task.is_archive_expired(retention_days) {
            permanently_delete_archived_task(config, task)?;
            purged += 1;
        }
    }

    Ok(purged)
}

/// List archived tasks with their TASK.md content loaded.
///
/// Returns (task, task_md_content) pairs for use in the archive view.
pub fn list_archived_tasks(config: &Config) -> Vec<(Task, String)> {
    Task::list_archived(config)
        .into_iter()
        .map(|task| {
            let content = task.read_task().unwrap_or_default();
            (task, content)
        })
        .collect()
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

/// Mark a stopped task as seen (user has viewed it in the preview).
///
/// This is the pure business logic behind "mark as read" when the user
/// navigates into the Preview view for a stopped task.
pub fn mark_task_seen(task: &mut Task) -> Result<()> {
    if task.meta.seen {
        return Ok(());
    }
    tracing::info!(task_id = %task.meta.task_id(), "marking task as seen");
    task.meta.seen = true;
    task.save_meta()
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

/// Toggle hold status on a project.
pub fn toggle_project_hold(config: &Config, project_name: &str) -> Result<()> {
    let mut project = Project::load_by_name(config, project_name)?;
    let old_held = project.meta.held;
    project.meta.held = !old_held;
    project.save_meta()?;
    tracing::info!(
        project = %project_name,
        old_held = old_held,
        new_held = project.meta.held,
        "toggled project hold"
    );
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
    Ok(task.queued_item_count())
}

/// Queue a command on a running task.
pub fn queue_command(task: &Task, command_id: &str, branch: Option<&str>) -> Result<usize> {
    tracing::info!(task_id = %task.meta.task_id(), command_id, "queuing command");
    task.queue_command(command_id, branch)?;
    Ok(task.queued_item_count())
}

/// Write immediate feedback for a stopped task.
///
/// Extracts the "stopped" branch of `App::submit_feedback()`.
/// Does NOT run `agman continue` — that's a side effect handled by the caller.
pub fn write_immediate_feedback(task: &Task, feedback: &str) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "writing immediate feedback");
    task.write_feedback(feedback)
}

/// Delete a single queued item by index.
pub fn delete_queue_item(task: &Task, index: usize) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), index, "deleting queued item");
    task.remove_queue_item(index)
}

/// Clear all queued items.
pub fn clear_queue(task: &Task) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), "clearing all queued items");
    task.clear_queue()
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

/// Pop the first queued item and apply it if it's feedback.
///
/// This is the pure business logic behind `App::process_stranded_queue()`.
/// For `QueueItem::Feedback`, writes FEEDBACK.md. For `QueueItem::Command`, just returns it.
/// The caller decides what side effects to perform (run continue vs run-command).
/// Returns the popped item, or None if the queue was empty.
pub fn pop_and_apply_queue_item(task: &Task) -> Result<Option<QueueItem>> {
    tracing::info!(task_id = %task.meta.task_id(), "popping and applying queued item");
    match task.pop_queue()? {
        Some(item) => {
            if let QueueItem::Feedback { ref text } = item {
                task.write_feedback(text)?;
            }
            Ok(Some(item))
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
pub fn set_linked_pr(task: &mut Task, pr_number: u64, worktree_path: &PathBuf, owned: bool, author: Option<String>) -> Result<()> {
    tracing::info!(task_id = %task.meta.task_id(), pr_number, owned, author = ?author, "setting linked PR");
    let remote_url = Git::get_remote_url(worktree_path)?;
    let (owner, repo) = git::parse_github_owner_repo(&remote_url)
        .ok_or_else(|| anyhow::anyhow!("Not a GitHub remote: {}", remote_url))?;
    let url = format!("https://github.com/{}/{}/pull/{}", owner, repo, pr_number);
    task.set_linked_pr(pr_number, url, owned, author)
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
    parent_dir: Option<PathBuf>,
    project: Option<String>,
) -> Result<Task> {
    tracing::info!(
        repo = repo_name,
        branch = branch_name,
        "creating setup-only task"
    );
    let parent_dir_ref = parent_dir.as_deref();

    // Initialize default files (flows, prompts, commands)
    config.init_default_files(false)?;

    // Set up or reuse worktree
    let worktree_path = match worktree_source {
        WorktreeSource::ExistingWorktree(path) => {
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::NewBranch { base_branch } => {
            let path = Git::create_worktree_quiet(config, repo_name, branch_name, base_branch.as_deref(), parent_dir_ref)?;
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::ExistingBranch => {
            let path =
                Git::create_worktree_for_existing_branch_quiet(config, repo_name, branch_name, parent_dir_ref)?;
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

    // Store parent_dir if repo is outside repos_dir
    if parent_dir.is_some() {
        task.meta.parent_dir = parent_dir;
    }

    // Assign to project if specified
    if project.is_some() {
        task.meta.project = project;
    }

    // Save if any optional fields were set after creation
    if task.meta.parent_dir.is_some() || task.meta.project.is_some() {
        task.save_meta()?;
    }

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
    parent_dir: Option<PathBuf>,
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
        parent_dir,
        None,
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

/// Migrate old-format `meta.json` files in-place to the new multi-repo format.
///
/// Old format had `repo_name`, `tmux_session`, `worktree_path` at the top level.
/// New format renames `repo_name` to `name` and nests per-repo fields inside `repos`.
///
/// Also copies TASK.md from the worktree to the task directory if it only exists
/// in the worktree (pre-refactor location).
///
/// This runs at TUI startup and is a one-time migration — once files are rewritten,
/// they stay in the new format.
pub fn migrate_old_tasks(config: &Config) {
    if !config.tasks_dir.exists() {
        return;
    }

    let read_dir = match std::fs::read_dir(&config.tasks_dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read tasks dir for migration");
            return;
        }
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }

        let dir_name = entry.file_name().to_string_lossy().to_string();
        let task_dir = entry.path();

        if let Err(e) = migrate_single_task(&task_dir, &dir_name) {
            tracing::warn!(task_id = %dir_name, error = %e, "failed to migrate old task");
        }
    }
}

fn migrate_single_task(task_dir: &std::path::Path, dir_name: &str) -> anyhow::Result<()> {
    let meta_path = task_dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&meta_path)?;
    let mut val: serde_json::Value = serde_json::from_str(&content)?;

    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return Ok(()),
    };

    // Detect old format: has "repo_name" at top level AND does NOT have "repos"
    let has_repo_name = obj.contains_key("repo_name");
    let has_repos = obj.contains_key("repos");

    if !has_repo_name || has_repos {
        tracing::debug!(task_id = %dir_name, "skipping task (already new format or unrecognized)");
        return Ok(());
    }

    // Extract old values before mutation
    let repo_name = obj
        .get("repo_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let worktree_path = obj
        .get("worktree_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tmux_session = obj
        .get("tmux_session")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Transform: rename repo_name → name
    obj.insert("name".to_string(), serde_json::Value::String(repo_name.clone()));
    obj.remove("repo_name");

    // Transform: create repos array from old top-level fields
    let repo_entry = serde_json::json!({
        "repo_name": repo_name,
        "worktree_path": worktree_path,
        "tmux_session": tmux_session,
    });
    obj.insert("repos".to_string(), serde_json::Value::Array(vec![repo_entry]));
    obj.remove("tmux_session");
    obj.remove("worktree_path");

    // Add parent_dir: null if missing
    if !obj.contains_key("parent_dir") {
        obj.insert("parent_dir".to_string(), serde_json::Value::Null);
    }

    // Write back
    let new_content = serde_json::to_string_pretty(&val)?;
    std::fs::write(&meta_path, new_content)?;

    tracing::info!(task_id = %dir_name, "migrated old-format meta.json");

    // TASK.md migration: copy from worktree to task dir if missing
    let task_md_in_dir = task_dir.join("TASK.md");
    if !task_md_in_dir.exists() && !worktree_path.is_empty() {
        let worktree_task_md = PathBuf::from(&worktree_path).join("TASK.md");
        if worktree_task_md.exists() {
            std::fs::copy(&worktree_task_md, &task_md_in_dir)?;
            tracing::info!(task_id = %dir_name, "copied TASK.md from worktree to task dir");
        }
    }

    Ok(())
}

/// Post-hook: read `# Repos` from TASK.md, create worktrees + tmux sessions,
/// and populate `task.meta.repos`.
///
/// Called after the repo-inspector agent finishes (via the `setup_repos` post-hook).
pub fn setup_repos_from_task_md(config: &Config, task: &mut Task, skip_tmux: bool) -> Result<()> {
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

    let parent_dir = task.meta.parent_dir.as_deref();

    for repo_name in &repo_names {
        // Check if worktree already exists (idempotent on retry after partial failure)
        let candidate = config.worktree_path_for(parent_dir, repo_name, &task.meta.branch_name);
        let worktree_path = if candidate.exists() {
            tracing::info!(repo = repo_name, branch = %task.meta.branch_name, "worktree already exists, reusing");
            let _ = Git::direnv_allow(&candidate);
            candidate
        } else {
            let path = Git::create_worktree_quiet(config, repo_name, &task.meta.branch_name, None, parent_dir)
                .with_context(|| format!("Failed to create worktree for repo '{}'", repo_name))?;
            let _ = Git::direnv_allow(&path);
            path
        };

        // Copy configured files (e.g. .env) from main repo to worktree (best-effort)
        if let Err(e) = copy_repo_files_to_worktree(config, repo_name, &worktree_path, parent_dir) {
            tracing::warn!(repo = repo_name, branch = %task.meta.branch_name, error = %e, "failed to copy repo files to worktree");
        }

        let tmux_session = Config::tmux_session_name(repo_name, &task.meta.branch_name);
        if !skip_tmux && !Tmux::session_exists(&tmux_session) {
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

/// Classify a directory path as a git repo, multi-repo parent, or plain directory.
///
/// - **GitRepo**: the directory contains `.git`
/// - **MultiRepoParent**: the directory contains at least one child directory with `.git`
/// - **Plain**: neither of the above
///
/// If a directory is both a git repo AND contains git-repo children,
/// it is classified as a git repo (`.git` takes priority).
pub fn classify_directory(path: &std::path::Path) -> DirKind {
    if path.join(".git").exists() {
        return DirKind::GitRepo;
    }
    if let Ok(read_dir) = std::fs::read_dir(path) {
        let has_git_children = read_dir
            .filter_map(|e| e.ok())
            .any(|e| e.path().is_dir() && e.path().join(".git").exists());
        if has_git_children {
            return DirKind::MultiRepoParent;
        }
    }
    DirKind::Plain
}

/// Classification of a directory for repo selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirKind {
    /// A git repository (has `.git`).
    GitRepo,
    /// A directory containing git-repo children.
    MultiRepoParent,
    /// A plain directory.
    Plain,
}


// ---------------------------------------------------------------------------
// Break Interval Settings
// ---------------------------------------------------------------------------

const DEFAULT_BREAK_INTERVAL_MINS: u64 = 40;

/// Load the break interval from config, defaulting to 40 minutes.
pub fn load_break_interval(config: &Config) -> Duration {
    let cf = crate::config::load_config_file(&config.base_dir);
    let mins = cf.break_interval_mins.unwrap_or(DEFAULT_BREAK_INTERVAL_MINS);
    Duration::from_secs(mins * 60)
}

/// Save the break interval to config, preserving other config fields.
pub fn save_break_interval(config: &Config, mins: u64) -> Result<()> {
    let mut cf = crate::config::load_config_file(&config.base_dir);
    cf.break_interval_mins = Some(mins);
    crate::config::save_config_file(&config.base_dir, &cf)
}

// ---------------------------------------------------------------------------
// Archive Retention Settings
// ---------------------------------------------------------------------------

const DEFAULT_ARCHIVE_RETENTION_DAYS: u64 = 30;

/// Load the archive retention period from config, defaulting to 30 days.
pub fn load_archive_retention(config: &Config) -> u64 {
    let cf = crate::config::load_config_file(&config.base_dir);
    cf.archive_retention_days.unwrap_or(DEFAULT_ARCHIVE_RETENTION_DAYS)
}

/// Save the archive retention period to config, preserving other config fields.
pub fn save_archive_retention(config: &Config, days: u64) -> Result<()> {
    let mut cf = crate::config::load_config_file(&config.base_dir);
    cf.archive_retention_days = Some(days);
    crate::config::save_config_file(&config.base_dir, &cf)
}

// ---------------------------------------------------------------------------
// Telegram Config
// ---------------------------------------------------------------------------

/// Load the Telegram bot token and chat ID from config.
pub fn load_telegram_config(config: &Config) -> (Option<String>, Option<String>) {
    let cf = crate::config::load_config_file(&config.base_dir);
    (
        cf.telegram_bot_token.map(|s| s.trim().to_string()),
        cf.telegram_chat_id.map(|s| s.trim().to_string()),
    )
}

/// Save the Telegram bot token and chat ID to config, preserving other config fields.
pub fn save_telegram_config(
    config: &Config,
    token: Option<String>,
    chat_id: Option<String>,
) -> Result<()> {
    let mut cf = crate::config::load_config_file(&config.base_dir);
    cf.telegram_bot_token = token;
    cf.telegram_chat_id = chat_id;
    crate::config::save_config_file(&config.base_dir, &cf)
}

// ---------------------------------------------------------------------------
// GitHub Notifications
// ---------------------------------------------------------------------------

/// A single GitHub notification, parsed from the `GET /notifications` API response.
#[derive(Debug, Clone)]
pub struct GithubNotification {
    pub id: String,
    pub repo_full_name: String,
    pub title: String,
    pub reason: String,
    pub subject_type: String,
    pub updated_at: String,
    pub unread: bool,
    pub browser_url: String,
}

/// Raw JSON shape returned by `GET /notifications` (subset of fields we care about).
#[derive(Deserialize)]
struct RawNotification {
    id: String,
    repository: RawRepo,
    subject: RawSubject,
    reason: String,
    updated_at: String,
    unread: bool,
}

#[derive(Deserialize)]
struct RawRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct RawSubject {
    title: String,
    url: Option<String>,
    #[serde(rename = "type")]
    subject_type: String,
}

/// Convert a GitHub API URL to a browser-openable URL.
///
/// Transforms:
/// - `https://api.github.com/repos/owner/repo/pulls/42` → `https://github.com/owner/repo/pull/42`
/// - `https://api.github.com/repos/owner/repo/issues/7` → `https://github.com/owner/repo/issues/7`
/// - `https://api.github.com/repos/owner/repo/commits/abc` → `https://github.com/owner/repo/commit/abc`
///
/// Falls back to `fallback` if `api_url` is empty.
pub fn api_url_to_browser_url(api_url: &str, fallback: &str) -> String {
    if api_url.is_empty() {
        return fallback.to_string();
    }
    let url = api_url
        .replace("https://api.github.com/repos/", "https://github.com/")
        .replace("/pulls/", "/pull/")
        .replace("/commits/", "/commit/");
    url
}

/// Parse the JSON response from `gh api /notifications` into a vec of notifications.
pub fn parse_notifications_json(json_str: &str) -> Vec<GithubNotification> {
    let raw: Vec<RawNotification> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse notifications JSON");
            return Vec::new();
        }
    };

    raw.into_iter()
        .map(|n| {
            let fallback = format!("https://github.com/{}", n.repository.full_name);
            let browser_url = api_url_to_browser_url(
                n.subject.url.as_deref().unwrap_or(""),
                &fallback,
            );
            GithubNotification {
                id: n.id,
                repo_full_name: n.repository.full_name,
                title: n.subject.title,
                reason: n.reason,
                subject_type: n.subject.subject_type,
                updated_at: n.updated_at,
                unread: n.unread,
                browser_url,
            }
        })
        .collect()
}

/// Result of a GitHub notifications poll.
pub struct NotifPollResult {
    pub notifications: Vec<GithubNotification>,
}

/// Fetch all GitHub notifications via paginated `gh api /notifications?all=true` calls.
///
/// Always performs a fresh fetch (no conditional requests). Paginates with
/// `per_page=50` (the API maximum) up to 10 pages (500 notifications max).
/// Limits results to notifications from the past N weeks (see `NOTIFICATION_RETENTION_WEEKS`).
pub fn fetch_github_notifications() -> NotifPollResult {
    use crate::dismissed_notifications::NOTIFICATION_RETENTION_WEEKS;

    let since = (chrono::Utc::now()
        - chrono::Duration::weeks(NOTIFICATION_RETENTION_WEEKS))
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    tracing::info!(since = %since, "fetching github notifications with time bound");

    let mut all_notifications = Vec::new();

    for page in 1..=10 {
        let url = format!("/notifications?all=true&per_page=50&page={page}&since={since}");
        let output = match Command::new("gh").args(["api", &url]).output() {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, page, "failed to run gh api for notifications");
                break;
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(stderr = %stderr, page, "gh api /notifications returned error");
            break;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let page_notifs = parse_notifications_json(&stdout);
        let count = page_notifs.len();
        all_notifications.extend(page_notifs);

        // Last page: fewer than per_page items means no more pages
        if count < 50 {
            break;
        }
    }

    tracing::debug!(total = all_notifications.len(), "fetched github notifications");
    NotifPollResult {
        notifications: all_notifications,
    }
}

/// Mark a GitHub notification thread as done (removes it from inbox).
pub fn dismiss_github_notification(thread_id: &str) -> Result<()> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("/notifications/threads/{}", thread_id),
            "--method",
            "DELETE",
        ])
        .output()
        .context("failed to run gh api DELETE notification")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to dismiss notification {}: {}", thread_id, stderr);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Keybase Unread Messages
// ---------------------------------------------------------------------------

/// Result of a Keybase unread-conversations poll.
pub struct KeybasePollResult {
    /// Number of DM conversations with unread messages (high priority).
    pub dm_unread_count: usize,
    /// Number of team channel conversations with unread messages (normal priority).
    pub channel_unread_count: usize,
    /// Whether the `keybase` binary was found. When `false`, the TUI should
    /// disable future polls.
    pub keybase_available: bool,
}

#[derive(Deserialize)]
struct RawKeybaseResponse {
    result: RawKeybaseResult,
}

#[derive(Deserialize)]
struct RawKeybaseResult {
    conversations: Option<Vec<RawKeybaseConversation>>,
}

#[derive(Deserialize)]
struct RawKeybaseChannel {
    members_type: Option<String>,
    topic_name: Option<String>,
}

#[derive(Deserialize)]
struct RawKeybaseConversation {
    unread: bool,
    channel: Option<RawKeybaseChannel>,
}

/// Poll Keybase for conversations with unread messages.
///
/// Shells out to `keybase chat api` with `unread_only: true`. Returns the
/// count of conversations that have unreads. Degrades gracefully: returns
/// zero with `keybase_available: false` if the binary is not found, or zero
/// with `keybase_available: true` if the daemon is temporarily unavailable.
pub fn fetch_keybase_unreads() -> KeybasePollResult {
    let payload = r#"{"method": "list", "params": {"options": {"unread_only": true}}}"#;

    let output = match Command::new("keybase")
        .args(["chat", "api", "-m", payload])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!("keybase binary not found, disabling poll");
            return KeybasePollResult {
                dm_unread_count: 0,
                channel_unread_count: 0,
                keybase_available: false,
            };
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to run keybase chat api");
            return KeybasePollResult {
                dm_unread_count: 0,
                channel_unread_count: 0,
                keybase_available: true,
            };
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(stderr = %stderr, "keybase chat api returned error");
        return KeybasePollResult {
            dm_unread_count: 0,
            channel_unread_count: 0,
            keybase_available: true,
        };
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: RawKeybaseResponse = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse keybase chat api response");
            return KeybasePollResult {
                dm_unread_count: 0,
                channel_unread_count: 0,
                keybase_available: true,
            };
        }
    };

    let (dm_unread_count, channel_unread_count) = parsed
        .result
        .conversations
        .as_ref()
        .map(|convos| {
            let mut dm = 0usize;
            let mut channel = 0usize;
            for c in convos.iter().filter(|c| c.unread) {
                let is_dm = c
                    .channel
                    .as_ref()
                    .map(|ch| {
                        ch.members_type.as_deref() == Some("impteamnative")
                            && ch.topic_name.is_none()
                    })
                    .unwrap_or(false);
                if is_dm {
                    dm += 1;
                } else {
                    channel += 1;
                }
            }
            (dm, channel)
        })
        .unwrap_or((0, 0));

    tracing::debug!(unread_dm = dm_unread_count, unread_channel = channel_unread_count, "keybase unread poll complete");
    KeybasePollResult {
        dm_unread_count,
        channel_unread_count,
        keybase_available: true,
    }
}

// ---------------------------------------------------------------------------
// Notes (standalone markdown files in ~/.agman/notes/)
// ---------------------------------------------------------------------------

/// A single entry in the notes file explorer.
#[derive(Debug, Clone)]
pub struct NoteEntry {
    /// Display name (no `.md` extension for files).
    pub name: String,
    /// Actual filename on disk.
    pub file_name: String,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

/// List directory contents for the notes explorer.
///
/// Returns directories first, then `.md` files, each group sorted alphabetically.
/// Non-`.md` files are excluded. The `.md` extension is stripped from display names.
/// If a `.order` file exists in the directory, entries are returned in that order
/// with any unmentioned entries appended alphabetically (dirs first).
pub fn list_notes(dir: &Path) -> Result<Vec<NoteEntry>> {
    let read_dir = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read notes directory: {}", dir.display()))?;

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in read_dir {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            dirs.push(NoteEntry {
                name: file_name.clone(),
                file_name,
                is_dir: true,
            });
        } else if file_type.is_file() && file_name.ends_with(".md") {
            let display_name = file_name.strip_suffix(".md").unwrap_or(&file_name).to_string();
            files.push(NoteEntry {
                name: display_name,
                file_name,
                is_dir: false,
            });
        }
    }

    // Check for .order file
    let order_path = dir.join(".order");
    if order_path.exists() {
        let content = std::fs::read_to_string(&order_path)
            .with_context(|| format!("failed to read .order file: {}", order_path.display()))?;
        let order_lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();

        // Collect all entries into a lookup map by file_name
        let mut all_entries: std::collections::HashMap<String, NoteEntry> = std::collections::HashMap::new();
        for e in dirs.into_iter().chain(files.into_iter()) {
            all_entries.insert(e.file_name.clone(), e);
        }

        // Build result: ordered entries first
        let mut result = Vec::new();
        for name in &order_lines {
            if let Some(entry) = all_entries.remove(*name) {
                result.push(entry);
            }
            // Silently skip .order entries that no longer exist on disk
        }

        // Append remaining entries not in .order (dirs first, then files, alphabetically)
        let mut remaining_dirs: Vec<NoteEntry> = Vec::new();
        let mut remaining_files: Vec<NoteEntry> = Vec::new();
        for entry in all_entries.into_values() {
            if entry.is_dir {
                remaining_dirs.push(entry);
            } else {
                remaining_files.push(entry);
            }
        }
        remaining_dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        remaining_files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        result.extend(remaining_dirs);
        result.extend(remaining_files);

        return Ok(result);
    }

    // Default: dirs first, then files, each group sorted alphabetically
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    dirs.extend(files);
    Ok(dirs)
}

/// Create a new `.md` note file in the given directory.
pub fn create_note(dir: &Path, name: &str) -> Result<PathBuf> {
    let file_name = if name.ends_with(".md") {
        name.to_string()
    } else {
        format!("{}.md", name)
    };
    let path = dir.join(&file_name);
    std::fs::write(&path, "")
        .with_context(|| format!("failed to create note: {}", path.display()))?;
    tracing::info!(note_path = %path.display(), "created note");
    Ok(path)
}

/// Create a new directory inside the notes tree.
pub fn create_note_dir(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to create note directory: {}", path.display()))?;
    tracing::info!(note_path = %path.display(), "created note directory");
    Ok(path)
}

/// Delete a note file or directory (recursive for directories).
pub fn delete_note(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to delete note directory: {}", path.display()))?;
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to delete note: {}", path.display()))?;
    }
    tracing::info!(note_path = %path.display(), "deleted note");
    Ok(())
}

/// Rename a note file or directory in-place (same parent directory).
///
/// For files, if `new_name` doesn't end in `.md`, the extension is appended automatically.
pub fn rename_note(old: &Path, new_name: &str) -> Result<PathBuf> {
    let parent = old.parent().context("note has no parent directory")?;
    let is_dir = old.is_dir();

    let actual_name = if !is_dir && !new_name.ends_with(".md") {
        format!("{}.md", new_name)
    } else {
        new_name.to_string()
    };

    let new_path = parent.join(&actual_name);
    std::fs::rename(old, &new_path)
        .with_context(|| format!("failed to rename {} to {}", old.display(), new_path.display()))?;
    tracing::info!(note_path = %new_path.display(), old_path = %old.display(), "renamed note");
    Ok(new_path)
}

/// Read the contents of a note file.
pub fn read_note(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read note: {}", path.display()))
}

/// Save content to a note file.
pub fn save_note(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content)
        .with_context(|| format!("failed to save note: {}", path.display()))
}

/// Direction for moving a note entry in the explorer.
#[derive(Debug, Clone, Copy)]
pub enum MoveDirection {
    Up,
    Down,
}

/// Move a note entry up or down within its directory.
///
/// Reads or initialises a `.order` file in the given directory to persist custom
/// ordering. Returns the new index of the moved entry so the caller can update
/// the selection cursor.
pub fn move_note(dir: &Path, file_name: &str, direction: MoveDirection) -> Result<usize> {
    let order_path = dir.join(".order");

    // Build current order: from .order file (reconciled with disk) or from list_notes.
    let mut order: Vec<String> = if order_path.exists() {
        let content = std::fs::read_to_string(&order_path)?;
        let mut from_file: Vec<String> = content.lines().filter(|l| !l.is_empty()).map(String::from).collect();
        // Reconcile: append any disk entries not already in .order
        let disk_entries = list_notes(dir)?;
        for entry in &disk_entries {
            if !from_file.contains(&entry.file_name) {
                from_file.push(entry.file_name.clone());
            }
        }
        // Remove .order entries that no longer exist on disk
        let disk_names: std::collections::HashSet<String> = disk_entries.into_iter().map(|e| e.file_name).collect();
        from_file.retain(|name| disk_names.contains(name));
        from_file
    } else {
        list_notes(dir)?.iter().map(|e| e.file_name.clone()).collect()
    };

    let idx = order.iter().position(|n| n == file_name)
        .with_context(|| format!("entry '{}' not found in order list", file_name))?;

    let new_idx = match direction {
        MoveDirection::Up => {
            if idx == 0 { return Ok(idx); }
            order.swap(idx, idx - 1);
            idx - 1
        }
        MoveDirection::Down => {
            if idx + 1 >= order.len() { return Ok(idx); }
            order.swap(idx, idx + 1);
            idx + 1
        }
    };

    // Write back
    let content = order.join("\n") + "\n";
    std::fs::write(&order_path, content)
        .with_context(|| format!("failed to write .order file: {}", order_path.display()))?;

    tracing::info!(dir = %dir.display(), file_name, direction = ?direction, "moved note");
    Ok(new_idx)
}

/// Move a note file or directory from one directory to another (cut-paste).
///
/// Performs a filesystem rename from `source_dir/file_name` to `dest_dir/file_name`.
/// Updates `.order` files in both source and destination directories if they exist.
/// Returns an error if the destination already contains an entry with the same name.
pub fn paste_note(source_dir: &Path, dest_dir: &Path, file_name: &str) -> Result<()> {
    let src_path = source_dir.join(file_name);
    let dest_path = dest_dir.join(file_name);

    if dest_path.exists() {
        anyhow::bail!("'{}' already exists in destination", file_name);
    }

    std::fs::rename(&src_path, &dest_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            src_path.display(),
            dest_path.display()
        )
    })?;

    // Remove from source .order if it exists
    let src_order = source_dir.join(".order");
    if src_order.exists() {
        let content = std::fs::read_to_string(&src_order)?;
        let lines: Vec<&str> = content
            .lines()
            .filter(|l| !l.is_empty() && *l != file_name)
            .collect();
        std::fs::write(&src_order, lines.join("\n") + "\n")?;
    }

    // Append to destination .order if it exists
    let dest_order = dest_dir.join(".order");
    if dest_order.exists() {
        let mut content = std::fs::read_to_string(&dest_order)?;
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str(file_name);
        content.push('\n');
        std::fs::write(&dest_order, content)?;
    }

    tracing::info!(
        src = %src_path.display(),
        dest = %dest_path.display(),
        "pasted note"
    );
    Ok(())
}

pub fn mark_notification_read(thread_id: &str) -> Result<()> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("/notifications/threads/{}", thread_id),
            "--method",
            "PATCH",
        ])
        .output()
        .context("failed to run gh api PATCH notification")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to mark notification {} as read: {}",
            thread_id,
            stderr
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Show PRs (GitHub Issues & PRs for current user)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum GithubItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone)]
pub struct GithubItem {
    pub number: u64,
    pub title: String,
    pub repo_full_name: String,
    pub state: String,
    pub url: String,
    pub updated_at: String,
    pub author: String,
    pub is_draft: bool,
    pub kind: GithubItemKind,
}

#[derive(Debug, Clone, Default)]
pub struct ShowPrsData {
    pub issues: Vec<GithubItem>,
    pub my_prs: Vec<GithubItem>,
    pub review_requests: Vec<GithubItem>,
}

/// Raw JSON shape from `gh search issues/prs --json ...`.
#[derive(Deserialize)]
struct RawSearchItem {
    number: u64,
    title: String,
    repository: RawSearchRepo,
    state: String,
    url: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    author: RawSearchAuthor,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
}

#[derive(Deserialize)]
struct RawSearchRepo {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct RawSearchAuthor {
    login: String,
}

/// Parse JSON output from `gh search issues` or `gh search prs`.
pub fn parse_search_items_json(json_str: &str, kind: GithubItemKind) -> Vec<GithubItem> {
    let raw: Vec<RawSearchItem> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse search items JSON");
            return Vec::new();
        }
    };

    raw.into_iter()
        .map(|item| GithubItem {
            number: item.number,
            title: item.title,
            repo_full_name: item.repository.name_with_owner,
            state: item.state,
            url: item.url,
            updated_at: item.updated_at,
            author: item.author.login,
            is_draft: if kind == GithubItemKind::Issue { false } else { item.is_draft },
            kind: kind.clone(),
        })
        .collect()
}

/// Run a `gh search` command and return stdout on success, or None on failure.
fn run_gh_search(args: &[&str]) -> Option<String> {
    let output = match Command::new("gh").args(args).output() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, cmd = ?args, "failed to run gh search");
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(stderr = %stderr, cmd = ?args, "gh search returned error");
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Deduplicate items by (number, repo_full_name), keeping first occurrence.
fn dedup_github_items(items: &mut Vec<GithubItem>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert((item.number, item.repo_full_name.clone())));
}

const PR_JSON_FIELDS: &str = "number,title,repository,state,url,updatedAt,author,isDraft";
const ISSUE_JSON_FIELDS: &str = "number,title,repository,state,url,updatedAt,author";

/// Fetch all GitHub issues and PRs relevant to the current user.
pub fn fetch_show_prs_data() -> ShowPrsData {
    tracing::info!("fetching show-prs data");

    // 1. My Issues (assigned to me)
    let issues = run_gh_search(&[
        "search", "issues", "--assignee=@me", "--state=open",
        &format!("--json={}", ISSUE_JSON_FIELDS), "--limit=50",
    ])
    .map(|json| parse_search_items_json(&json, GithubItemKind::Issue))
    .unwrap_or_default();

    // 2. My PRs (authored by me)
    let mut my_prs = run_gh_search(&[
        "search", "prs", "--author=@me", "--state=open",
        &format!("--json={}", PR_JSON_FIELDS), "--limit=50",
    ])
    .map(|json| parse_search_items_json(&json, GithubItemKind::PullRequest))
    .unwrap_or_default();

    // 3. PRs assigned to me (merge into my_prs)
    if let Some(json) = run_gh_search(&[
        "search", "prs", "--assignee=@me", "--state=open",
        &format!("--json={}", PR_JSON_FIELDS), "--limit=50",
    ]) {
        my_prs.extend(parse_search_items_json(&json, GithubItemKind::PullRequest));
        dedup_github_items(&mut my_prs);
    }

    // 4. Review requests
    let mut review_requests = run_gh_search(&[
        "search", "prs", "--review-requested=@me", "--state=open",
        &format!("--json={}", PR_JSON_FIELDS), "--limit=50",
    ])
    .map(|json| parse_search_items_json(&json, GithubItemKind::PullRequest))
    .unwrap_or_default();

    // 5. PRs mentioning me (merge into review_requests)
    if let Some(json) = run_gh_search(&[
        "search", "prs", "--mentions=@me", "--state=open",
        &format!("--json={}", PR_JSON_FIELDS), "--limit=50",
    ]) {
        review_requests.extend(parse_search_items_json(&json, GithubItemKind::PullRequest));
        dedup_github_items(&mut review_requests);
    }

    tracing::debug!(
        issues = issues.len(),
        my_prs = my_prs.len(),
        review_requests = review_requests.len(),
        "fetched show-prs data"
    );

    ShowPrsData {
        issues,
        my_prs,
        review_requests,
    }
}

// ---------------------------------------------------------------------------
// Project management
// ---------------------------------------------------------------------------

/// Create a new project with the given name and description.
pub fn create_project(config: &Config, name: &str, description: &str) -> Result<Project> {
    tracing::info!(project = name, "creating project");
    let project = Project::create(config, name, description)?;

    // Eagerly start PM session for the new project
    if let Err(e) = start_pm_session(config, name) {
        tracing::error!(project = name, error = %e, "failed to start PM session for new project");
    }

    Ok(project)
}

/// List all projects.
pub fn list_projects(config: &Config) -> Result<Vec<Project>> {
    Project::list_all(config)
}

/// Summary info for a project.
pub struct ProjectStatusInfo {
    pub project: Project,
    pub total_tasks: usize,
    pub active_tasks: usize,
    pub archived_tasks: usize,
}

/// Get detailed status of a project.
pub fn project_status(config: &Config, name: &str) -> Result<ProjectStatusInfo> {
    let project = Project::load_by_name(config, name)?;
    let tasks = list_project_tasks(config, name)?;
    let active_tasks = tasks
        .iter()
        .filter(|t| t.meta.status == TaskStatus::Running)
        .count();
    let archived_tasks = Task::list_archived(config)
        .iter()
        .filter(|t| t.meta.project.as_deref() == Some(name))
        .count();

    Ok(ProjectStatusInfo {
        project,
        total_tasks: tasks.len(),
        active_tasks,
        archived_tasks,
    })
}

/// List tasks belonging to a project.
pub fn list_project_tasks(config: &Config, project_name: &str) -> Result<Vec<Task>> {
    let all = Task::list_all(config);
    Ok(all
        .into_iter()
        .filter(|t| t.meta.project.as_deref() == Some(project_name))
        .collect())
}

/// List tasks not assigned to any project.
pub fn list_unassigned_tasks(config: &Config) -> Result<Vec<Task>> {
    let all = Task::list_all(config);
    Ok(all.into_iter().filter(|t| t.meta.project.is_none()).collect())
}

/// Delete a project: archive all its tasks, kill PM session, remove project directory.
pub fn delete_project(config: &Config, project_name: &str) -> Result<()> {
    // Verify the project exists
    let _project = Project::load_by_name(config, project_name)?;

    // Archive all non-archived tasks
    let tasks = list_project_tasks(config, project_name)?;
    let mut archived_count = 0;
    for mut task in tasks {
        if task.meta.archived_at.is_some() {
            continue;
        }
        // Kill tmux sessions for repos (best-effort)
        if task.meta.has_repos() {
            for repo in &task.meta.repos {
                let _ = Tmux::kill_session(&repo.tmux_session);
            }
        }
        // Kill multi-repo parent session if applicable
        if task.meta.is_multi_repo() {
            let parent_session =
                Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
            let _ = Tmux::kill_session(&parent_session);
        }
        archive_task(config, &mut task, false)?;
        archived_count += 1;
    }

    // Archive all non-archived researchers
    let researchers = Researcher::list_for_project(config, project_name)?;
    let mut researcher_archived_count = 0;
    for researcher in &researchers {
        if researcher.meta.status == crate::researcher::ResearcherStatus::Archived {
            continue;
        }
        if let Err(e) = archive_researcher(config, &researcher.meta.project, &researcher.meta.name) {
            tracing::warn!(
                project = %project_name,
                researcher = %researcher.meta.name,
                error = %e,
                "failed to archive researcher during project deletion"
            );
        } else {
            researcher_archived_count += 1;
        }
    }

    // Kill PM tmux session (best-effort)
    let _ = Tmux::kill_session(&Config::pm_tmux_session(project_name));

    // Remove project directory
    std::fs::remove_dir_all(config.project_dir(project_name))?;

    tracing::info!(project = %project_name, archived_count, researcher_archived_count, "deleted project");

    Ok(())
}

// ---------------------------------------------------------------------------
// Aggregated status
// ---------------------------------------------------------------------------

/// Summary of a single task for the aggregated status view.
pub struct TaskSummary {
    pub task_id: String,
    pub status: TaskStatus,
    pub flow_step: usize,
    pub total_steps: Option<usize>,
    pub current_agent: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub queued_count: usize,
}

/// Summary of a researcher for the aggregated status view.
pub struct ResearcherSummary {
    pub name: String,
    pub project: String,
    pub status: String,
    pub description: String,
}

/// A group of tasks belonging to a project.
pub struct ProjectGroup {
    pub name: String,
    pub tasks: Vec<TaskSummary>,
    pub archived_count: usize,
    pub researchers: Vec<ResearcherSummary>,
    pub held: bool,
}

/// Aggregated status across all projects and tasks.
pub struct AggregatedStatus {
    pub projects: Vec<ProjectGroup>,
    pub unassigned: Vec<TaskSummary>,
    pub archived_unassigned: usize,
    pub ceo_researchers: Vec<ResearcherSummary>,
}

fn task_to_summary(config: &Config, task: &Task) -> TaskSummary {
    let total_steps = Flow::load(&config.flow_path(&task.meta.flow_name))
        .ok()
        .map(|f| f.steps.len());

    TaskSummary {
        task_id: task.meta.task_id(),
        status: task.meta.status,
        flow_step: task.meta.flow_step,
        total_steps,
        current_agent: task.meta.current_agent.clone(),
        updated_at: task.meta.updated_at,
        queued_count: task.queued_item_count(),
    }
}

/// Build researcher summaries for a given project, filtering out archived ones.
fn load_researcher_summaries(config: &Config, project: &str) -> Vec<ResearcherSummary> {
    let researchers = match Researcher::list_for_project(config, project) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    researchers
        .into_iter()
        .filter(|r| r.meta.status != ResearcherStatus::Archived)
        .map(|r| {
            let session = Config::researcher_tmux_session(&r.meta.project, &r.meta.name);
            let status = if Tmux::session_exists(&session) {
                "running"
            } else {
                "stopped"
            };
            ResearcherSummary {
                name: r.meta.name,
                project: r.meta.project,
                status: status.to_string(),
                description: r.meta.description,
            }
        })
        .collect()
}

/// Get aggregated status across all projects and their tasks.
pub fn aggregated_status(config: &Config) -> Result<AggregatedStatus> {
    tracing::info!("computing aggregated status");

    let archived = Task::list_archived(config);
    let projects = list_projects(config)?;
    let mut project_groups = Vec::new();

    for project in &projects {
        if project.meta.held {
            continue;
        }

        let tasks = list_project_tasks(config, &project.meta.name)?;
        let summaries: Vec<TaskSummary> = tasks.iter().map(|t| task_to_summary(config, t)).collect();
        let researchers = load_researcher_summaries(config, &project.meta.name);

        // Skip projects with no active tasks and no active researchers
        if summaries.is_empty() && researchers.is_empty() {
            continue;
        }

        let archived_count = archived
            .iter()
            .filter(|t| t.meta.project.as_deref() == Some(&project.meta.name))
            .count();
        project_groups.push(ProjectGroup {
            name: project.meta.name.clone(),
            tasks: summaries,
            archived_count,
            researchers,
            held: project.meta.held,
        });
    }

    let unassigned_tasks = list_unassigned_tasks(config)?;
    let unassigned: Vec<TaskSummary> = unassigned_tasks
        .iter()
        .map(|t| task_to_summary(config, t))
        .collect();

    let archived_unassigned = archived
        .iter()
        .filter(|t| t.meta.project.is_none())
        .count();

    let ceo_researchers = load_researcher_summaries(config, "ceo");

    Ok(AggregatedStatus {
        projects: project_groups,
        unassigned,
        archived_unassigned,
        ceo_researchers,
    })
}

// ---------------------------------------------------------------------------
// Task migration
// ---------------------------------------------------------------------------

/// Migrate tasks to a project by setting their `project` field.
/// The target project must already exist. Returns the number of tasks migrated.
pub fn migrate_tasks_to_project(
    config: &Config,
    project_name: &str,
    task_ids: &[String],
) -> Result<usize> {
    // Verify the target project exists
    let _project = Project::load_by_name(config, project_name)
        .with_context(|| format!("target project '{}' does not exist", project_name))?;

    let mut migrated = 0;
    for task_id in task_ids {
        let task_dir = config.tasks_dir.join(task_id);
        if !task_dir.exists() {
            tracing::warn!(task_id = %task_id, "task not found, skipping");
            continue;
        }
        let meta_path = task_dir.join("meta.json");
        let load_result: Result<Task> = (|| {
            let content = std::fs::read_to_string(&meta_path)
                .context("failed to read meta.json")?;
            let meta: crate::task::TaskMeta =
                serde_json::from_str(&content).context("failed to parse meta.json")?;
            Ok(Task { meta, dir: task_dir.clone() })
        })();
        match load_result {
            Ok(mut task) => {
                task.meta.project = Some(project_name.to_string());
                task.save_meta()?;
                tracing::info!(task_id = %task_id, project = project_name, "migrated task to project");
                migrated += 1;
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to load task, skipping");
            }
        }
    }

    Ok(migrated)
}

// ---------------------------------------------------------------------------
// Task creation with project
// ---------------------------------------------------------------------------

/// Create a task within a project. Similar to `create_task` but sets the project field.
pub fn create_pm_task(
    config: &Config,
    project: &str,
    repo_name: &str,
    branch_name: &str,
    description: &str,
) -> Result<Task> {
    tracing::info!(
        project = project,
        repo = repo_name,
        branch = branch_name,
        "creating PM task"
    );

    // Verify project exists
    let _project = Project::load_by_name(config, project)?;

    // Create the task using the standard create_task function
    let task = create_task(
        config,
        repo_name,
        branch_name,
        description,
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        Some(project.to_string()),
    )?;

    Ok(task)
}

// ---------------------------------------------------------------------------
// Messaging
// ---------------------------------------------------------------------------

/// Send a message to an agent's inbox.
pub fn send_message(config: &Config, target: &str, from: &str, message: &str) -> Result<()> {
    let inbox_path = if target == "ceo" {
        config.ceo_inbox()
    } else if target == "telegram" {
        config.telegram_outbox()
    } else if let Some(researcher_id) = target.strip_prefix("researcher:") {
        let (project, name) = parse_researcher_id(researcher_id)?;
        config.researcher_inbox(project, name)
    } else {
        config.project_inbox(target)
    };

    tracing::info!(target = target, from = from, "sending message");
    inbox::append_message(&inbox_path, from, message)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent session handoff / respawn
// ---------------------------------------------------------------------------

/// Append a handoff request message to an agent's inbox.
pub fn request_handoff(inbox_path: &Path, from: &str, state_dir: &Path) -> Result<()> {
    let message = format!(
        "[HANDOFF REQUEST] Your session is being replaced. Before it ends, write a summary of your current state to {}/handoff.md. Include: current objectives, recent actions, pending items, and important context. No other action needed — just write the file.",
        state_dir.display()
    );
    inbox::append_message(inbox_path, from, &message)?;
    Ok(())
}

/// Poll for handoff completion by watching for `handoff.md` to appear and stabilize.
/// Returns `Some(content)` if `handoff.md` was written, `None` on timeout with no content.
pub fn wait_for_handoff(
    target: &str,
    handoff_path: &Path,
    timeout_secs: u64,
) -> Result<Option<String>> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_secs(2);
    let mut last_size: Option<u64> = None;

    loop {
        if start.elapsed() >= timeout {
            tracing::warn!(
                target = target,
                "handoff timed out after {}s",
                timeout_secs
            );
            // Check if partial handoff.md exists
            if handoff_path.exists() {
                if let Ok(content) = std::fs::read_to_string(handoff_path) {
                    if !content.trim().is_empty() {
                        tracing::info!(target = target, "using partial handoff.md after timeout");
                        return Ok(Some(content));
                    }
                }
            }
            return Ok(None);
        }

        // Check if handoff.md exists and is non-empty
        if let Ok(meta) = std::fs::metadata(handoff_path) {
            let size = meta.len();
            if size > 0 {
                if let Some(prev) = last_size {
                    if prev == size {
                        // File size stable across two polls — read and return
                        if let Ok(content) = std::fs::read_to_string(handoff_path) {
                            if !content.trim().is_empty() {
                                tracing::info!(target = target, "handoff complete");
                                return Ok(Some(content));
                            }
                        }
                    }
                }
                last_size = Some(size);
            }
        }

        std::thread::sleep(poll_interval);
    }
}

/// Respawn an agent (CEO or PM) with a fresh session.
/// If `force` is false and the session is running, performs a graceful handoff first.
pub fn respawn_agent(
    config: &Config,
    target: &str,
    force: bool,
    timeout_secs: u64,
) -> Result<()> {
    if target.starts_with("researcher:") {
        tracing::info!(target = target, "rejected researcher respawn attempt");
        bail!("Respawning researchers is not supported. Create a new researcher instead.");
    }

    // Parse target to determine agent type and resolve paths
    let (state_dir, session_name, inbox_path, session_id_path) = if target == "ceo" {
        (
            config.ceo_dir(),
            Config::ceo_tmux_session().to_string(),
            config.ceo_inbox(),
            config.ceo_session_id(),
        )
    } else {
        // PM for a project
        (
            config.project_dir(target),
            Config::pm_tmux_session(target),
            config.project_inbox(target),
            config.project_session_id(target),
        )
    };

    let handoff_path = state_dir.join("handoff.md");
    tracing::info!(target = target, force = force, "respawning agent");

    // Delete old handoff.md to avoid stale content
    let _ = std::fs::remove_file(&handoff_path);

    let mut handoff_content: Option<String> = None;

    // Graceful handoff if not forced and session is running
    if !force && Tmux::session_exists(&session_name) {
        tracing::info!(target = target, "requesting graceful handoff");
        request_handoff(&inbox_path, "system", &state_dir)?;
        handoff_content = wait_for_handoff(target, &handoff_path, timeout_secs)?;
    }

    // Kill old session
    if Tmux::session_exists(&session_name) {
        Tmux::kill_session(&session_name)?;
    }

    // Delete session-id to force fresh session
    let _ = std::fs::remove_file(&session_id_path);

    // Start new session
    if target == "ceo" {
        start_ceo_session(config)?;
    } else {
        start_pm_session(config, target)?;
    }

    // Inject handoff content to new session
    if let Some(content) = handoff_content {
        let message = format!("[HANDOFF FROM PREVIOUS SESSION] The following is a context summary from your predecessor session. Integrate this into your understanding and continue where they left off.\n\n{content}");
        inbox::append_message(&inbox_path, "system", &message)?;
        tracing::info!(target = target, "injected handoff context to new session");
    }

    // Cleanup handoff.md
    let _ = std::fs::remove_file(&handoff_path);

    tracing::info!(target = target, "agent respawned successfully");
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent session management
// ---------------------------------------------------------------------------

/// Start the CEO agent session.
pub fn start_ceo_session(config: &Config) -> Result<()> {
    let ceo_dir = config.ceo_dir();
    std::fs::create_dir_all(&ceo_dir).context("failed to create CEO directory")?;

    // Check for resume ID
    let session_id_path = config.ceo_session_id();
    let resume_id = std::fs::read_to_string(&session_id_path).ok();
    let resume_ref = resume_id.as_deref().map(|s| s.trim());

    let (token, chat_id) = load_telegram_config(config);
    let telegram_enabled = token.as_deref().is_some_and(|t| !t.is_empty())
        && chat_id.as_deref().is_some_and(|c| !c.is_empty());
    let prompt = build_ceo_prompt(telegram_enabled);

    let session_name = Config::ceo_tmux_session();
    tracing::info!(session = session_name, telegram_enabled, "starting CEO session");
    Tmux::create_agent_session(session_name, &prompt, resume_ref, None, None)?;
    Ok(())
}

/// Open a CEO chat as a tmux popup overlaid on the current pane.
/// Ensures the persistent CEO session is running, then attaches a popup to it.
pub fn open_ceo_popup(config: &Config) -> Result<()> {
    start_ceo_session(config)?;
    tracing::info!("opening CEO popup");
    Tmux::popup_attach(Config::ceo_tmux_session())?;
    Ok(())
}

/// Start a PM agent session for a project.
pub fn start_pm_session(config: &Config, project_name: &str) -> Result<()> {
    let project_dir = config.project_dir(project_name);
    if !project_dir.exists() {
        anyhow::bail!("project '{}' does not exist", project_name);
    }

    let session_id_path = config.project_session_id(project_name);
    let resume_id = std::fs::read_to_string(&session_id_path).ok();
    let resume_ref = resume_id.as_deref().map(|s| s.trim());

    let prompt = DEFAULT_PM_PROMPT_TEMPLATE.replace("{{PROJECT_NAME}}", project_name);
    let session_name = Config::pm_tmux_session(project_name);
    tracing::info!(session = &session_name, project = project_name, "starting PM session");
    Tmux::create_agent_session(&session_name, &prompt, resume_ref, None, None)?;
    Ok(())
}

/// Open a PM chat as a tmux popup overlaid on the current pane.
/// Ensures the persistent PM session is running, then attaches a popup to it.
pub fn open_pm_popup(config: &Config, project_name: &str) -> Result<()> {
    start_pm_session(config, project_name)?;
    let session_name = Config::pm_tmux_session(project_name);
    tracing::info!(project = project_name, "opening PM popup");
    Tmux::popup_attach(&session_name)?;
    Ok(())
}

/// Check if an agent's tmux session is running.
pub fn agent_session_running(session_name: &str) -> bool {
    Tmux::session_exists(session_name)
}

// ---------------------------------------------------------------------------
// Researcher management
// ---------------------------------------------------------------------------

/// Create a new researcher for a project.
pub fn create_researcher(
    config: &Config,
    project: &str,
    name: &str,
    description: &str,
    repo: Option<String>,
    branch: Option<String>,
    task_id: Option<String>,
) -> Result<Researcher> {
    // Validate project exists (skip for CEO-level researchers)
    if project != "ceo" {
        let project_dir = config.project_dir(project);
        if !project_dir.exists() {
            anyhow::bail!("project '{}' does not exist", project);
        }
    }

    tracing::info!(project = project, name = name, "creating researcher");
    let researcher = Researcher::create(config, project, name, description, repo, branch, task_id)?;

    // Write the research description to the inbox so the TUI poller delivers it
    // to the tmux session once Claude Code is ready (instead of direct injection).
    if !description.is_empty() {
        let inbox_path = config.researcher_inbox(project, name);
        crate::inbox::append_message(&inbox_path, "user", description)?;
        tracing::debug!(project = project, name = name, "queued research description to inbox");
    }

    Ok(researcher)
}

/// Start a researcher's Claude Code tmux session.
pub fn start_researcher_session(config: &Config, project: &str, name: &str) -> Result<()> {
    let dir = config.researcher_dir(project, name);
    let researcher = Researcher::load(dir)?;

    let session_id_path = config.researcher_session_id(project, name);
    let existing_session_id = std::fs::read_to_string(&session_id_path).ok();
    let existing_session_id_trimmed = existing_session_id.as_deref().map(|s| s.trim().to_string());

    // Decide fresh-creation vs resume based on whether a session-id file exists
    let (resume_id, new_session_id) = if let Some(ref id) = existing_session_id_trimmed {
        // Resume: pass the existing session ID as resume_id
        (Some(id.as_str()), None)
    } else {
        // Fresh creation: generate a UUID and persist it for future resumes
        let uuid = uuid::Uuid::new_v4().to_string();
        if let Some(parent) = session_id_path.parent() {
            std::fs::create_dir_all(parent).context("failed to create researcher dir")?;
        }
        std::fs::write(&session_id_path, &uuid)
            .context("failed to write researcher session-id")?;
        tracing::info!(project = project, name = name, session_id = %uuid, "generated new researcher session ID");
        (None, Some(uuid))
    };

    let template = if project == "ceo" {
        DEFAULT_CEO_RESEARCHER_PROMPT_TEMPLATE
    } else {
        DEFAULT_RESEARCHER_PROMPT_TEMPLATE
    };
    let prompt = template
        .replace("{{PROJECT_NAME}}", project)
        .replace("{{RESEARCHER_NAME}}", name);

    // Resolve working directory
    let work_dir = resolve_researcher_work_dir(config, &researcher);

    let session_name = Config::researcher_tmux_session(project, name);
    tracing::info!(session = &session_name, project = project, name = name, "starting researcher session");
    Tmux::create_agent_session(
        &session_name,
        &prompt,
        resume_id,
        new_session_id.as_deref(),
        work_dir.as_deref(),
    )?;

    Ok(())
}

/// Resolve the working directory for a researcher session.
fn resolve_researcher_work_dir(config: &Config, researcher: &Researcher) -> Option<PathBuf> {
    // If task_id is set, try to resolve to the task's worktree path
    if let Some(ref task_id) = researcher.meta.task_id {
        // task_id is "<repo>--<branch>" format
        if let Some((repo, branch)) = task_id.split_once("--") {
            if let Ok(task) = Task::load(config, repo, branch) {
                if let Some(repo_entry) = task.meta.repos.first() {
                    let wt_path = PathBuf::from(&repo_entry.worktree_path);
                    if wt_path.exists() {
                        return Some(wt_path);
                    }
                }
            }
        }
    }

    // If repo + branch, resolve to worktree
    if let (Some(ref repo), Some(ref branch)) = (&researcher.meta.repo, &researcher.meta.branch) {
        let wt_dir = config.repos_dir.parent()?.join(format!("{repo}-wt")).join(branch);
        if wt_dir.exists() {
            return Some(wt_dir);
        }
    }

    // If repo only, resolve to main repo dir
    if let Some(ref repo) = researcher.meta.repo {
        let repo_dir = config.repos_dir.join(repo);
        if repo_dir.exists() {
            return Some(repo_dir);
        }
    }

    None
}

/// List researchers, optionally filtered by project.
pub fn list_researchers(config: &Config, project: Option<&str>) -> Result<Vec<Researcher>> {
    match project {
        Some(p) => Researcher::list_for_project(config, p),
        None => Researcher::list_all(config),
    }
}

/// Archive a researcher (kill tmux session, set status to Archived).
pub fn archive_researcher(config: &Config, project: &str, name: &str) -> Result<()> {
    let dir = config.researcher_dir(project, name);
    let mut researcher = Researcher::load(dir)?;

    let session_name = Config::researcher_tmux_session(project, name);
    if Tmux::session_exists(&session_name) {
        tracing::info!(session = &session_name, "killing researcher tmux session");
        Tmux::kill_session(&session_name)?;
    }

    researcher.meta.status = crate::researcher::ResearcherStatus::Archived;
    researcher.save_meta()?;
    tracing::info!(project = project, name = name, "researcher archived");
    Ok(())
}

/// Resume an archived researcher: start a new tmux session (with --resume) and flip status to Running.
pub fn resume_researcher(config: &Config, project: &str, name: &str) -> Result<()> {
    start_researcher_session(config, project, name)?;

    let dir = config.researcher_dir(project, name);
    let mut researcher = Researcher::load(dir)?;
    researcher.meta.status = crate::researcher::ResearcherStatus::Running;
    researcher.save_meta()?;

    tracing::info!(project = project, name = name, "researcher resumed");
    Ok(())
}

/// Parse a researcher ID of the form "<project>--<name>" into (project, name).
pub fn parse_researcher_id(id: &str) -> Result<(&str, &str)> {
    let pos = id
        .find("--")
        .ok_or_else(|| anyhow::anyhow!("invalid researcher id '{}': expected '<project>--<name>'", id))?;
    Ok((&id[..pos], &id[pos + 2..]))
}

// ---------------------------------------------------------------------------
// Task query (for CLI commands)
// ---------------------------------------------------------------------------

/// Format a chrono::Duration into a human-readable string like "2m", "1h 23m".
fn format_duration(duration: chrono::Duration) -> String {
    let total_secs = duration.num_seconds().max(0);
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

/// Extract the Goal section from a TASK.md file.
/// Returns None if the file doesn't exist or has no `# Goal` section.
/// Truncates to the first 5 lines or 500 characters, whichever comes first.
fn extract_goal_summary(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    // Find the "# Goal" heading
    let mut found = false;
    for line in lines.by_ref() {
        if line.trim() == "# Goal" {
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    // Collect lines until the next heading or EOF
    let mut goal_lines = Vec::new();
    let mut char_count = 0;
    for line in lines {
        if line.starts_with('#') {
            break;
        }
        // Skip leading blank lines
        if goal_lines.is_empty() && line.trim().is_empty() {
            continue;
        }
        if char_count + line.len() + 1 > 500 {
            // Line would exceed 500 chars — stop without including partial line
            break;
        }
        char_count += line.len() + 1; // +1 for newline
        goal_lines.push(line);
        if goal_lines.len() >= 5 {
            break;
        }
    }
    if goal_lines.is_empty() {
        return None;
    }
    Some(goal_lines.join("\n"))
}

/// Get a formatted status string for a task.
pub fn get_task_status_text(config: &Config, task_id: &str) -> Result<String> {
    let (repo, branch) = Config::parse_task_id(task_id)
        .context(format!("invalid task ID: {}", task_id))?;
    let task = Task::load(config, &repo, &branch)?;

    let mut out = String::new();
    out.push_str(&format!("Task: {}\n", task_id));
    out.push_str(&format!("Status: {}\n", task.meta.status));

    if let Some(archived_at) = &task.meta.archived_at {
        let suffix = if task.meta.saved { " (saved)" } else { "" };
        out.push_str(&format!("Archived: {}{}\n", archived_at, suffix));
    }

    // Rich flow step display
    let flow_line = match Flow::load(&config.flow_path(&task.meta.flow_name)) {
        Ok(flow) => {
            let total = flow.steps.len();
            let step = task.meta.flow_step;
            if let Some(flow_step) = flow.get_step(step) {
                let agent_name = match flow_step {
                    FlowStep::Agent(s) => s.agent.clone(),
                    FlowStep::Loop(l) => {
                        let agents: Vec<&str> =
                            l.steps.iter().map(|s| s.agent.as_str()).collect();
                        format!("loop: {}", agents.join(" → "))
                    }
                };
                format!(
                    "Flow: {} (step {}/{}: {})\n",
                    task.meta.flow_name,
                    step + 1,
                    total,
                    agent_name,
                )
            } else {
                format!(
                    "Flow: {} (step {}/{})\n",
                    task.meta.flow_name,
                    step + 1,
                    total,
                )
            }
        }
        Err(_) => format!(
            "Flow: {} (step {})\n",
            task.meta.flow_name, task.meta.flow_step + 1,
        ),
    };
    out.push_str(&flow_line);

    if let Some(ref agent) = task.meta.current_agent {
        out.push_str(&format!("Agent: {}\n", agent));
    }
    if let Some(ref project) = task.meta.project {
        out.push_str(&format!("Project: {}\n", project));
    }
    out.push_str(&format!("Created: {}\n", task.meta.created_at));
    out.push_str(&format!("Updated: {}\n", task.meta.updated_at));

    // Elapsed time for running tasks
    if task.meta.status == TaskStatus::Running {
        let elapsed = Utc::now().signed_duration_since(task.meta.updated_at);
        out.push_str(&format!("Running for: {}\n", format_duration(elapsed)));
    }

    // Queue info
    let queue = task.read_queue();
    if queue.is_empty() {
        out.push_str("Queue: empty\n");
    } else {
        out.push_str(&format!("Queue: {} item{}\n", queue.len(), if queue.len() == 1 { "" } else { "s" }));
        for item in &queue {
            match item {
                QueueItem::Feedback { text } => {
                    let truncated = if text.chars().count() > 50 {
                        format!("{}...", text.chars().take(50).collect::<String>())
                    } else {
                        text.clone()
                    };
                    out.push_str(&format!("  [feedback] {}\n", truncated));
                }
                QueueItem::Command { command_id, branch } => {
                    let branch_str = branch.as_deref().map(|b| format!(" ({})", b)).unwrap_or_default();
                    out.push_str(&format!("  [cmd] {}{}\n", command_id, branch_str));
                }
            }
        }
    }

    // Task goal from TASK.md
    if let Some(goal) = extract_goal_summary(&task.dir.join("TASK.md")) {
        out.push_str(&format!("\nGoal:\n{}\n", goal));
    }

    // Append last few lines of agent log
    let log_tail = get_task_log_tail(config, task_id, 10)?;
    if !log_tail.is_empty() {
        out.push_str("\n--- Recent log ---\n");
        out.push_str(&log_tail);
    }

    Ok(out)
}

/// Read the full contents of a task's TASK.md.
pub fn get_task_current_plan(config: &Config, task_id: &str) -> Result<String> {
    let (repo, branch) = Config::parse_task_id(task_id)
        .context(format!("invalid task ID: {}", task_id))?;
    let task = Task::load(config, &repo, &branch)?;
    let plan_path = task.dir.join("TASK.md");

    if !plan_path.exists() {
        return Ok(String::new());
    }

    let contents = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("failed to read {}", plan_path.display()))?;

    Ok(contents)
}

/// Read the last N lines of a task's agent.log.
pub fn get_task_log_tail(config: &Config, task_id: &str, n: usize) -> Result<String> {
    let (repo, branch) = Config::parse_task_id(task_id)
        .context(format!("invalid task ID: {}", task_id))?;
    let task = Task::load(config, &repo, &branch)?;
    let log_path = task.dir.join("agent.log");

    if !log_path.exists() {
        return Ok(String::new());
    }

    let contents = std::fs::read_to_string(&log_path)
        .with_context(|| format!("failed to read {}", log_path.display()))?;

    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].join("\n"))
}

// ---------------------------------------------------------------------------
// Chat unread tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatLastSeen(pub std::collections::HashMap<String, u64>);

impl ChatLastSeen {
    pub fn load(path: &Path) -> Self {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Self::default(),
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }

    pub fn get(&self, key: &str) -> u64 {
        self.0.get(key).copied().unwrap_or(0)
    }

    pub fn set(&mut self, key: String, seq: u64) {
        self.0.insert(key, seq);
    }
}

pub struct ChatPollResult {
    pub unread_names: Vec<String>,
}

/// Collect names of inboxes with unread chat messages (CEO + all projects).
pub fn count_unread_chat_messages(config: &Config) -> ChatPollResult {
    let last_seen_path = config.chat_last_seen_path();
    let last_seen = ChatLastSeen::load(&last_seen_path);
    tracing::debug!(path = %last_seen_path.display(), contents = ?last_seen, "loaded chat_last_seen");
    let mut unread_names: Vec<String> = Vec::new();

    // CEO inbox
    let ceo_inbox = config.ceo_inbox();
    let ceo_exists = ceo_inbox.exists();
    match inbox::read_messages(&ceo_inbox) {
        Ok(messages) => {
            let message_count = messages.len();
            if let Some(last) = messages.last() {
                let seen = last_seen.get("ceo");
                tracing::debug!(
                    path = %ceo_inbox.display(),
                    exists = ceo_exists,
                    message_count,
                    last_seq = last.seq,
                    seen_seq = seen,
                    inbox_key = "ceo",
                    "CEO inbox check"
                );
                if last.seq > seen {
                    unread_names.push("CEO".to_string());
                }
            } else {
                tracing::debug!(
                    path = %ceo_inbox.display(),
                    exists = ceo_exists,
                    message_count = 0,
                    inbox_key = "ceo",
                    "CEO inbox empty"
                );
            }
        }
        Err(e) => {
            tracing::debug!(
                path = %ceo_inbox.display(),
                exists = ceo_exists,
                error = %e,
                inbox_key = "ceo",
                "CEO inbox read error"
            );
        }
    }

    // Project inboxes
    let projects_dir = config.projects_dir();
    match std::fs::read_dir(&projects_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let project_name = match entry.file_name().into_string() {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let inbox_path = config.project_inbox(&project_name);
                let inbox_key = format!("project:{}", project_name);
                let inbox_exists = inbox_path.exists();
                match inbox::read_messages(&inbox_path) {
                    Ok(messages) => {
                        let message_count = messages.len();
                        if let Some(last) = messages.last() {
                            let seen = last_seen.get(&inbox_key);
                            tracing::debug!(
                                path = %inbox_path.display(),
                                exists = inbox_exists,
                                message_count,
                                last_seq = last.seq,
                                seen_seq = seen,
                                %inbox_key,
                                "project inbox check"
                            );
                            if last.seq > seen {
                                unread_names.push(project_name);
                            }
                        } else {
                            tracing::debug!(
                                path = %inbox_path.display(),
                                exists = inbox_exists,
                                message_count = 0,
                                %inbox_key,
                                "project inbox empty"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            path = %inbox_path.display(),
                            exists = inbox_exists,
                            error = %e,
                            %inbox_key,
                            "project inbox read error"
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::debug!(
                path = %projects_dir.display(),
                error = %e,
                "projects dir read error"
            );
        }
    }

    tracing::debug!(?unread_names, "chat poll result");
    ChatPollResult { unread_names }
}

/// Mark a chat inbox as read by recording the current max seq in chat_last_seen.json.
pub fn mark_chat_read(config: &Config, inbox_key: &str, inbox_path: &Path) -> Result<()> {
    let messages = match inbox::read_messages(inbox_path) {
        Ok(m) => m,
        Err(_) => return Ok(()), // inbox doesn't exist, no-op
    };

    if let Some(last) = messages.last() {
        let path = config.chat_last_seen_path();
        let mut last_seen = ChatLastSeen::load(&path);
        last_seen.set(inbox_key.to_string(), last.seq);
        last_seen.save(&path);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Default system prompts
// ---------------------------------------------------------------------------


const DEFAULT_CEO_PROMPT: &str = r#"You are the CEO agent — the strategic orchestrator for agman. You delegate work to Project Managers (PMs), never implement anything yourself.

## Your Role
- Receive high-level goals and break them into projects
- Create projects and brief PMs on what needs to be done
- Monitor project progress and resolve cross-project issues
- Be concise and action-oriented

## Available Commands (use via Bash tool)

### Project Management
- `agman create-project <name> --description "<desc>"` — Create a new project with a PM
- `agman list-projects` — List all projects with PM status and task counts
- `agman project-status <name>` — Get detailed status of a project

### Communication
- Send a message to a PM (use heredoc to avoid shell escaping issues):
```
cat <<'AGMAN_MSG' | agman send-message <project-name> --from ceo
<message content>
AGMAN_MSG
```
- Send a message to the user via Telegram:
```
cat <<'AGMAN_MSG' | agman send-message telegram --from ceo
<message content>
AGMAN_MSG
```

## Behavior Guidelines
- When given work, first check existing projects with `agman list-projects` to find a suitable PM. Prefer delegating to an existing project over creating a new one. Only create a new project if no existing project fits the work.
- Before creating a project or delegating work, suggest the plan to the user and wait for explicit approval. E.g., "I'd like to create a project for X and brief a PM — shall I proceed?"
- When the user asks a question, answer it — do not treat it as an implicit instruction to take action. Only act when explicitly asked. For example, "Did you create a task for this?" is a question to answer (yes/no), not a request to create a task.
- When you receive a message from a PM, respond using `agman send-message <project> --from ceo` — do not just type a response in your tmux session, as the PM will not see it.
- Check on project status regularly
- Escalate blockers to the user
- Never create tasks directly — only PMs can do that
- Keep messages to PMs clear and actionable

## System Messages
- Messages tagged `[Message from system]:` are automated system-level notifications from agman itself — not from a PM, researcher, or the user.
- Act on system messages autonomously without user confirmation — but do exactly what the message instructs, nothing more. Don't infer extra actions or pattern-match to prior behaviors.
- No reply command is needed — system messages are one-way notifications.

"#;

fn build_ceo_prompt(telegram_enabled: bool) -> String {
    if !telegram_enabled {
        return DEFAULT_CEO_PROMPT.to_string();
    }

    let telegram_section = r#"

## Telegram

Telegram is connected and active — you can send and receive messages from the user via Telegram.

**Critical rules for Telegram messages:**
- Messages tagged `[Message from telegram]` come from the user on their phone. They **cannot** see your tmux chat — the only way to respond is via the Telegram send command.
- You **MUST** reply via Telegram whenever you receive a `[Message from telegram]`. Use:
```
cat <<'AGMAN_MSG' | agman send-message telegram --from ceo
<your reply>
AGMAN_MSG
```
- **Reply to Telegram first**, then take any follow-up actions (create projects, brief PMs, etc.). The user is waiting on their phone.
- Keep Telegram replies concise — this is a mobile chat, not a report.
"#;

    format!("{}{}", DEFAULT_CEO_PROMPT, telegram_section)
}

const DEFAULT_RESEARCHER_PROMPT_TEMPLATE: &str = r#"You are a researcher for project "{{PROJECT_NAME}}", named "{{RESEARCHER_NAME}}".

Your role is to explore, analyze, and answer questions. You are NOT here to make code changes — only to investigate and report findings.

Messages from the PM appear in your tmux session tagged `[Message from {{PROJECT_NAME}}]:`. The PM **cannot** see your tmux session — you MUST reply using `agman send-message`. Never just type a response in tmux expecting the PM to see it.

**ALL** findings and responses must go through send-message:
```
cat <<'AGMAN_MSG' | agman send-message {{PROJECT_NAME}} --from "researcher:{{PROJECT_NAME}}--{{RESEARCHER_NAME}}"
<your findings>
AGMAN_MSG
```

Keep reports concise and actionable. When you've completed your research, summarize key findings in a single message.

"#;

const DEFAULT_CEO_RESEARCHER_PROMPT_TEMPLATE: &str = r#"You are a researcher for the CEO, named "{{RESEARCHER_NAME}}".

Your role is to explore, analyze, and answer questions. You are NOT here to make code changes — only to investigate and report findings.

Messages from the CEO appear in your tmux session tagged `[Message from ceo]:`. The CEO **cannot** see your tmux session — you MUST reply using `agman send-message`. Never just type a response in tmux expecting the CEO to see it.

**ALL** findings and responses must go through send-message:
```
cat <<'AGMAN_MSG' | agman send-message ceo --from "researcher:ceo--{{RESEARCHER_NAME}}"
<your findings>
AGMAN_MSG
```

Keep reports concise and actionable. When you've completed your research, summarize key findings in a single message.

"#;

const DEFAULT_PM_PROMPT_TEMPLATE: &str = r#"You are the Project Manager (PM) for the "{{PROJECT_NAME}}" project in agman. You manage tasks to accomplish your project's goals.

## Your Role
- Receive goals from the CEO or user and break them into concrete tasks
- Create and monitor tasks within your project
- Report progress and issues back to the CEO
- Break goals into concrete, well-scoped tasks

## Available Commands (use via Bash tool)

### Task Management
- `agman create-pm-task {{PROJECT_NAME}} <repo> <task-name> --description "<description>"` — Create a new task
- `agman list-pm-tasks {{PROJECT_NAME}}` — List your project's tasks
- `agman task-status <task-id>` — Get task status and recent log
- `agman task-log <task-id> --tail 100` — Read task's agent log

### Communication
- Report to the CEO (use heredoc to avoid shell escaping issues):
```
cat <<'AGMAN_MSG' | agman send-message ceo --from {{PROJECT_NAME}}
<message content>
AGMAN_MSG
```

## Behavior Guidelines
- When given work, suggest a task plan to the CEO (or user, if addressed directly) and wait for confirmation before creating tasks.
- When the user asks a question, answer it — do not treat it as an implicit instruction to take action. Only act when explicitly asked. For example, "Did you create a task for this?" is a question to answer (yes/no), not a request to create a task.
- Monitor task progress and report completion to the CEO
- If a task fails, analyze the logs and either retry or escalate
- Never run long commands yourself — always spawn a task for implementation work
- Keep the CEO informed of significant progress or blockers

## Reactive Behavior
- **Relay completions — only for CEO-initiated work**: When you receive a notification that a task or researcher has completed, failed, or needs input, check whether this work originated from a CEO directive. If the CEO asked you to do this work, report the result back to the CEO via `send-message` — include the task name, outcome, and relevant details (e.g., PR link, error summary). If you initiated the work yourself (e.g., routine maintenance, self-directed improvements), do NOT report back to the CEO.
- **Do NOT poll**: Never proactively check task statuses or poll researchers. Tasks and researchers notify you when they finish — wait for those notifications. Your behavior is entirely event-driven: receive a message, then act.

## Message Routing

Messages from other agents appear in your tmux session tagged `[Message from <sender>]:`. The sender **cannot** see your tmux session — you MUST reply using `agman send-message`. Never just type a response in tmux.

**CEO messages** — tagged `[Message from ceo]:`
- Reply immediately via send-message, **then** take follow-up actions. The CEO is waiting.
```
cat <<'AGMAN_MSG' | agman send-message ceo --from {{PROJECT_NAME}}
<your reply>
AGMAN_MSG
```

**Researcher messages** — tagged `[Message from researcher:{{PROJECT_NAME}}--<name>]:`
- Extract the researcher name from the tag and reply via send-message.
```
cat <<'AGMAN_MSG' | agman send-message researcher:{{PROJECT_NAME}}--<researcher-name> --from {{PROJECT_NAME}}
<your reply>
AGMAN_MSG
```

**Direct user input** (no `[Message from ...]` tag) — respond directly in the tmux session. No routing needed.

"#;
