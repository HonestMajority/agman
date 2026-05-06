use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::assistant::{Assistant, AssistantKind, AssistantStatus, ReviewerWorktree};
use crate::config::Config;
use crate::flow::{Flow, FlowStep};
use crate::git::{self, Git};
use crate::harness::{self, HarnessKind, LaunchContext, RegisterContext, SessionKey};
use crate::inbox;
use crate::project::Project;
use crate::repo_stats::RepoStats;
use crate::supervisor;
use crate::task::{QueueItem, RepoEntry, Task, TaskStatus};
use crate::tmux::Tmux;

/// Required external tools that must be on $PATH (harness binary excluded —
/// it's resolved per-config via [`Config::default_harness`] and prepended
/// dynamically by [`check_dependencies`]).
const REQUIRED_TOOLS: &[&str] = &["tmux", "git", "nvim", "lazygit", "gh", "direnv"];

/// Check that all required external tools are present on $PATH. Includes
/// the configured harness binary. Returns a list of
/// missing tool names (empty if all present).
pub fn check_dependencies(config: &Config) -> Vec<String> {
    let harness = config.default_harness();
    let harness_bin = harness.cli_binary();
    let mut missing = Vec::new();
    let mut all_tools: Vec<&str> = Vec::with_capacity(REQUIRED_TOOLS.len() + 1);
    all_tools.push(harness_bin);
    all_tools.extend_from_slice(REQUIRED_TOOLS);
    for tool in all_tools {
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

/// Return an install hint for a missing tool. The configured harness binary
/// defers to the harness's `install_hint`.
pub fn install_hint(config: &Config, tool: &str) -> String {
    match tool {
        "tmux" => "brew install tmux (macOS) / apt install tmux (Linux)".into(),
        "git" => "brew install git (macOS) / apt install git (Linux)".into(),
        "nvim" => "brew install neovim (macOS) / apt install neovim (Linux)".into(),
        "lazygit" => {
            "brew install lazygit (macOS) / go install github.com/jesseduffield/lazygit@latest"
                .into()
        }
        "gh" => "brew install gh (macOS) / apt install gh (Linux)".into(),
        "direnv" => "brew install direnv (macOS) / apt install direnv (Linux)".into(),
        other => {
            let h = config.default_harness();
            if h.cli_binary() == other {
                h.install_hint().to_string()
            } else {
                "(see tool documentation)".to_string()
            }
        }
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
#[allow(clippy::too_many_arguments)]
pub fn create_task(
    config: &Config,
    repo_name: &str,
    branch_name: &str,
    description: &str,
    flow_name: &str,
    worktree_source: WorktreeSource,
    parent_dir: Option<PathBuf>,
    project: Option<String>,
) -> Result<Task> {
    tracing::info!(
        repo = repo_name,
        branch = branch_name,
        flow = flow_name,
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
                tracing::info!(
                    repo = repo_name,
                    branch = branch_name,
                    "worktree already exists, reusing"
                );
                let _ = Git::direnv_allow(&candidate);
                candidate
            } else {
                let path = Git::create_worktree_quiet(
                    config,
                    repo_name,
                    branch_name,
                    base_branch.as_deref(),
                    parent_dir_ref,
                )?;
                let _ = Git::direnv_allow(&path);
                path
            }
        }
        WorktreeSource::ExistingBranch => {
            let candidate = config.worktree_path_for(parent_dir_ref, repo_name, branch_name);
            if candidate.exists() {
                tracing::info!(
                    repo = repo_name,
                    branch = branch_name,
                    "worktree already exists, reusing"
                );
                let _ = Git::direnv_allow(&candidate);
                candidate
            } else {
                let path = Git::create_worktree_for_existing_branch_quiet(
                    config,
                    repo_name,
                    branch_name,
                    parent_dir_ref,
                )?;
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

    // Assign to project if specified
    if project.is_some() {
        task.meta.project = project;
    }

    // Save if any optional fields were set after creation
    if task.meta.parent_dir.is_some() || task.meta.project.is_some() {
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
    project: Option<String>,
) -> Result<Task> {
    tracing::info!(
        name = name,
        branch = branch_name,
        flow = flow_name,
        parent_dir = %parent_dir.display(),
        "creating multi-repo task"
    );

    // Initialize default files (flows, prompts, commands)
    config.init_default_files(false)?;

    // Create task files (no worktrees — repos not yet determined)
    let mut task = Task::create_multi(
        config,
        name,
        branch_name,
        description,
        flow_name,
        parent_dir,
    )?;

    // Assign to project if specified
    if project.is_some() {
        task.meta.project = project;
    }

    // Save if any optional fields were set after creation
    if task.meta.project.is_some() {
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

/// Stop a running task by driving the full supervisor stop pathway.
///
/// Routes through `supervisor::honor_stop`, which:
/// - Kills the currently-running harness in the agman pane (so it doesn't
///   stay orphaned after the user clicked stop).
/// - Finalizes the last `SessionEntry` with `stopped_at` + `condition =
///   STOPPED`.
/// - Restores any pre-command flow snapshot so the task is back to its
///   original flow position when stopped mid-stored-command.
/// - Clears the `.stop` and other sentinel files.
/// - Sets status to Stopped and clears `current_agent`.
///
/// This is the pure business logic behind `App::stop_task()`. The supervisor
/// pathway is preferred over a direct status flip because directly setting
/// status to Stopped would orphan the live harness in the agman window, leak
/// session-history entries with `stopped_at = null`, and leave stored-command
/// flow state stranded.
pub fn stop_task(config: &Config, task: &mut Task) -> Result<()> {
    if task.meta.status == TaskStatus::Stopped {
        return Ok(());
    }
    tracing::info!(
        task_id = %task.meta.task_id(),
        old_status = %task.meta.status,
        new_status = "stopped",
        "stopping task via supervisor::honor_stop"
    );
    crate::supervisor::honor_stop(config, task)
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

/// Queue feedback for a task and, if the task is currently idle (Stopped),
/// wake the supervisor so it drains the queue and launches the next step.
///
/// If `wake_if_idle` fails (for example, tmux is unavailable) the error is
/// logged but *not* propagated — the feedback is already persisted in the
/// queue, so the user's action should still be reported as successful.
/// A subsequent supervisor poll or user action can retry the launch.
pub fn queue_feedback(task: &mut Task, config: &Config, feedback: &str) -> Result<usize> {
    tracing::info!(task_id = %task.meta.task_id(), "queuing feedback");
    task.append_feedback_to_log(feedback)?;
    task.queue_feedback(feedback)?;
    let count = task.queued_item_count();
    if let Err(e) = supervisor::wake_if_idle(config, task) {
        tracing::warn!(
            task_id = %task.meta.task_id(),
            error = %e,
            "wake_if_idle failed after queuing feedback"
        );
    }
    Ok(count)
}

/// Queue a command for a task and, if the task is currently idle (Stopped),
/// wake the supervisor so it drains the queue and launches the command flow.
///
/// Error-swallowing semantics match `queue_feedback`.
pub fn queue_command(
    task: &mut Task,
    config: &Config,
    command_id: &str,
    branch: Option<&str>,
) -> Result<usize> {
    tracing::info!(task_id = %task.meta.task_id(), command_id, "queuing command");
    task.queue_command(command_id, branch)?;
    let count = task.queued_item_count();
    if let Err(e) = supervisor::wake_if_idle(config, task) {
        tracing::warn!(
            task_id = %task.meta.task_id(),
            error = %e,
            "wake_if_idle failed after queuing command"
        );
    }
    Ok(count)
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
pub fn set_linked_pr(
    task: &mut Task,
    pr_number: u64,
    worktree_path: &PathBuf,
    owned: bool,
    author: Option<String>,
) -> Result<()> {
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
            let path = Git::create_worktree_quiet(
                config,
                repo_name,
                branch_name,
                base_branch.as_deref(),
                parent_dir_ref,
            )?;
            let _ = Git::direnv_allow(&path);
            path
        }
        WorktreeSource::ExistingBranch => {
            let path = Git::create_worktree_for_existing_branch_quiet(
                config,
                repo_name,
                branch_name,
                parent_dir_ref,
            )?;
            let _ = Git::direnv_allow(&path);
            path
        }
    };

    // Create task files
    let mut task = Task::create(config, repo_name, branch_name, "", "none", worktree_path)?;

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
    task.write_task("# Goal\n")?;

    // Increment repo usage stats
    let stats_path = config.repo_stats_path();
    let mut stats = RepoStats::load(&stats_path);
    stats.increment(repo_name);
    stats.save(&stats_path);

    tracing::info!(task_id = %task.meta.task_id(), "setup-only task created");

    Ok(task)
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
    obj.insert(
        "name".to_string(),
        serde_json::Value::String(repo_name.clone()),
    );
    obj.remove("repo_name");

    // Transform: create repos array from old top-level fields
    let repo_entry = serde_json::json!({
        "repo_name": repo_name,
        "worktree_path": worktree_path,
        "tmux_session": tmux_session,
    });
    obj.insert(
        "repos".to_string(),
        serde_json::Value::Array(vec![repo_entry]),
    );
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
            let path = Git::create_worktree_quiet(
                config,
                repo_name,
                &task.meta.branch_name,
                None,
                parent_dir,
            )
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
            }
        }

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
// Archive Retention Settings
// ---------------------------------------------------------------------------

const DEFAULT_ARCHIVE_RETENTION_DAYS: u64 = 30;

/// Load the archive retention period from config, defaulting to 30 days.
pub fn load_archive_retention(config: &Config) -> u64 {
    let cf = crate::config::load_config_file(&config.base_dir);
    cf.archive_retention_days
        .unwrap_or(DEFAULT_ARCHIVE_RETENTION_DAYS)
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

/// Health classification for the Telegram bot thread, derived from the
/// in-memory heartbeat the bot writes each loop iteration.
#[derive(Debug, PartialEq, Eq)]
pub enum TelegramHealth {
    Disabled,
    NeverPolled,
    Healthy,
    Stale,
    Dead,
}

/// Classify Telegram bot health for the status-bar indicator.
///
/// Thresholds (vs. wall-clock now): `<30s` healthy, `30..=120s` stale, `>120s` dead.
/// `heartbeat_epoch == None` means the bot has never written a heartbeat
/// (callers should coerce sentinel `0` from the `AtomicU64` to `None`).
pub fn classify_telegram_health(
    heartbeat_epoch: Option<u64>,
    now_epoch: u64,
    configured: bool,
) -> TelegramHealth {
    if !configured {
        return TelegramHealth::Disabled;
    }
    let Some(hb) = heartbeat_epoch else {
        return TelegramHealth::NeverPolled;
    };
    let age = now_epoch.saturating_sub(hb);
    if age < 30 {
        TelegramHealth::Healthy
    } else if age <= 120 {
        TelegramHealth::Stale
    } else {
        TelegramHealth::Dead
    }
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
    api_url
        .replace("https://api.github.com/repos/", "https://github.com/")
        .replace("/pulls/", "/pull/")
        .replace("/commits/", "/commit/")
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
            let browser_url =
                api_url_to_browser_url(n.subject.url.as_deref().unwrap_or(""), &fallback);
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

    let since = (chrono::Utc::now() - chrono::Duration::weeks(NOTIFICATION_RETENTION_WEEKS))
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

    tracing::debug!(
        total = all_notifications.len(),
        "fetched github notifications"
    );
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

    tracing::debug!(
        unread_dm = dm_unread_count,
        unread_channel = channel_unread_count,
        "keybase unread poll complete"
    );
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
            let display_name = file_name
                .strip_suffix(".md")
                .unwrap_or(&file_name)
                .to_string();
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
        let mut all_entries: std::collections::HashMap<String, NoteEntry> =
            std::collections::HashMap::new();
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
    std::fs::rename(old, &new_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            old.display(),
            new_path.display()
        )
    })?;
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
        let mut from_file: Vec<String> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        // Reconcile: append any disk entries not already in .order
        let disk_entries = list_notes(dir)?;
        for entry in &disk_entries {
            if !from_file.contains(&entry.file_name) {
                from_file.push(entry.file_name.clone());
            }
        }
        // Remove .order entries that no longer exist on disk
        let disk_names: std::collections::HashSet<String> =
            disk_entries.into_iter().map(|e| e.file_name).collect();
        from_file.retain(|name| disk_names.contains(name));
        from_file
    } else {
        list_notes(dir)?
            .iter()
            .map(|e| e.file_name.clone())
            .collect()
    };

    let idx = order
        .iter()
        .position(|n| n == file_name)
        .with_context(|| format!("entry '{}' not found in order list", file_name))?;

    let new_idx = match direction {
        MoveDirection::Up => {
            if idx == 0 {
                return Ok(idx);
            }
            order.swap(idx, idx - 1);
            idx - 1
        }
        MoveDirection::Down => {
            if idx + 1 >= order.len() {
                return Ok(idx);
            }
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
            is_draft: if kind == GithubItemKind::Issue {
                false
            } else {
                item.is_draft
            },
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
        "search",
        "issues",
        "--assignee=@me",
        "--state=open",
        &format!("--json={}", ISSUE_JSON_FIELDS),
        "--limit=50",
    ])
    .map(|json| parse_search_items_json(&json, GithubItemKind::Issue))
    .unwrap_or_default();

    // 2. My PRs (authored by me)
    let mut my_prs = run_gh_search(&[
        "search",
        "prs",
        "--author=@me",
        "--state=open",
        &format!("--json={}", PR_JSON_FIELDS),
        "--limit=50",
    ])
    .map(|json| parse_search_items_json(&json, GithubItemKind::PullRequest))
    .unwrap_or_default();

    // 3. PRs assigned to me (merge into my_prs)
    if let Some(json) = run_gh_search(&[
        "search",
        "prs",
        "--assignee=@me",
        "--state=open",
        &format!("--json={}", PR_JSON_FIELDS),
        "--limit=50",
    ]) {
        my_prs.extend(parse_search_items_json(&json, GithubItemKind::PullRequest));
        dedup_github_items(&mut my_prs);
    }

    // 4. Review requests
    let mut review_requests = run_gh_search(&[
        "search",
        "prs",
        "--review-requested=@me",
        "--state=open",
        &format!("--json={}", PR_JSON_FIELDS),
        "--limit=50",
    ])
    .map(|json| parse_search_items_json(&json, GithubItemKind::PullRequest))
    .unwrap_or_default();

    // 5. PRs mentioning me (merge into review_requests)
    if let Some(json) = run_gh_search(&[
        "search",
        "prs",
        "--mentions=@me",
        "--state=open",
        &format!("--json={}", PR_JSON_FIELDS),
        "--limit=50",
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
pub fn create_project(
    config: &Config,
    name: &str,
    description: &str,
    initial_message: Option<&str>,
) -> Result<Project> {
    tracing::info!(project = name, "creating project");
    let project = Project::create(config, name, description)?;

    // Only the explicit initial-message goes to the PM. The description is a
    // human label only — never auto-sent.
    if let Some(msg) = initial_message.map(str::trim).filter(|m| !m.is_empty()) {
        let inbox_path = config.project_inbox(name);
        crate::inbox::append_message(&inbox_path, "chief-of-staff", msg)?;
        tracing::debug!(project = name, "queued initial message to project inbox");
    }

    // Eagerly start PM session for the new project
    if let Err(e) = start_pm_session(config, name, false) {
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
    let active_tasks = tasks.len();
    let archived_tasks = Task::list_archived(config)
        .iter()
        .filter(|t| t.meta.project.as_deref() == Some(name))
        .count();

    Ok(ProjectStatusInfo {
        project,
        total_tasks: active_tasks + archived_tasks,
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
    Ok(all
        .into_iter()
        .filter(|t| t.meta.project.is_none())
        .collect())
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
            let parent_session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
            let _ = Tmux::kill_session(&parent_session);
        }
        archive_task(config, &mut task, false)?;
        archived_count += 1;
    }

    // Archive all non-archived assistants (researchers and reviewers).
    let assistants = Assistant::list_for_project(config, project_name)?;
    let mut assistant_archived_count = 0;
    for assistant in &assistants {
        if assistant.meta.status == AssistantStatus::Archived {
            continue;
        }
        if let Err(e) = archive_assistant(config, &assistant.meta.project, &assistant.meta.name) {
            tracing::warn!(
                project = %project_name,
                assistant = %assistant.meta.name,
                error = %e,
                "failed to archive assistant during project deletion"
            );
        } else {
            assistant_archived_count += 1;
        }
    }

    // Kill PM tmux session (best-effort)
    let _ = Tmux::kill_session(&Config::pm_tmux_session(project_name));

    // Remove project directory
    std::fs::remove_dir_all(config.project_dir(project_name))?;

    tracing::info!(project = %project_name, archived_count, assistant_archived_count, "deleted project");

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

/// Summary of an assistant (researcher or reviewer) for the aggregated
/// status view. The kind is exposed so renderers can group/label as needed.
pub struct AssistantSummary {
    pub name: String,
    pub project: String,
    pub status: String,
    pub description: String,
    pub kind: AssistantKindLabel,
}

/// Lightweight kind discriminator for status views — avoids leaking the
/// full `AssistantKind` payload (which carries kind-specific metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantKindLabel {
    Researcher,
    Reviewer,
}

/// A group of tasks belonging to a project.
pub struct ProjectGroup {
    pub name: String,
    pub tasks: Vec<TaskSummary>,
    pub archived_count: usize,
    pub assistants: Vec<AssistantSummary>,
    pub held: bool,
}

/// Aggregated status across all projects and tasks.
pub struct AggregatedStatus {
    pub projects: Vec<ProjectGroup>,
    pub unassigned: Vec<TaskSummary>,
    pub archived_unassigned: usize,
    pub chief_of_staff_assistants: Vec<AssistantSummary>,
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

/// Build assistant summaries for a given project, filtering out archived ones.
fn load_assistant_summaries(config: &Config, project: &str) -> Vec<AssistantSummary> {
    let assistants = match Assistant::list_for_project(config, project) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    assistants
        .into_iter()
        .filter(|a| a.meta.status != AssistantStatus::Archived)
        .map(|a| {
            let (session, kind) = match a.meta.kind {
                AssistantKind::Researcher { .. } => (
                    Config::researcher_tmux_session(&a.meta.project, &a.meta.name),
                    AssistantKindLabel::Researcher,
                ),
                AssistantKind::Reviewer { .. } => (
                    Config::reviewer_tmux_session(&a.meta.project, &a.meta.name),
                    AssistantKindLabel::Reviewer,
                ),
            };
            let status = if Tmux::session_exists(&session) {
                "running"
            } else {
                "stopped"
            };
            AssistantSummary {
                name: a.meta.name,
                project: a.meta.project,
                status: status.to_string(),
                description: a.meta.description,
                kind,
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
        let summaries: Vec<TaskSummary> =
            tasks.iter().map(|t| task_to_summary(config, t)).collect();
        let assistants = load_assistant_summaries(config, &project.meta.name);

        // Skip projects with no active tasks and no active assistants
        if summaries.is_empty() && assistants.is_empty() {
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
            assistants,
            held: project.meta.held,
        });
    }

    let unassigned_tasks = list_unassigned_tasks(config)?;
    let unassigned: Vec<TaskSummary> = unassigned_tasks
        .iter()
        .map(|t| task_to_summary(config, t))
        .collect();

    let archived_unassigned = archived.iter().filter(|t| t.meta.project.is_none()).count();

    let chief_of_staff_assistants = load_assistant_summaries(config, "chief-of-staff");

    Ok(AggregatedStatus {
        projects: project_groups,
        unassigned,
        archived_unassigned,
        chief_of_staff_assistants,
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
            let content =
                std::fs::read_to_string(&meta_path).context("failed to read meta.json")?;
            let meta: crate::task::TaskMeta =
                serde_json::from_str(&content).context("failed to parse meta.json")?;
            Ok(Task {
                meta,
                dir: task_dir.clone(),
            })
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
        None,
        Some(project.to_string()),
    )?;

    Ok(task)
}

// ---------------------------------------------------------------------------
// Messaging
// ---------------------------------------------------------------------------

pub enum SendTarget {
    ChiefOfStaff,
    Telegram,
    Project(String),
    /// A long-lived assistant (researcher or reviewer). The kind is recorded
    /// on disk in `meta.json` and is irrelevant for routing — both prefixes
    /// resolve to the same `assistants/<project>--<name>/inbox.jsonl`.
    Assistant {
        project: String,
        name: String,
        prefix: AssistantPrefix,
    },
    Task(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantPrefix {
    Researcher,
    Reviewer,
}

impl AssistantPrefix {
    fn as_str(&self) -> &'static str {
        match self {
            AssistantPrefix::Researcher => "researcher",
            AssistantPrefix::Reviewer => "reviewer",
        }
    }
}

const VALID_TARGETS_HINT: &str =
    "valid targets: chief-of-staff, telegram, <project>, researcher:<project>--<name>, reviewer:<project>--<name>, task:<repo>--<branch>";

pub fn parse_send_target(config: &Config, target: &str) -> Result<SendTarget> {
    if target.is_empty() {
        anyhow::bail!("unknown target ''\n{VALID_TARGETS_HINT}");
    }

    if target == "chief-of-staff" {
        return Ok(SendTarget::ChiefOfStaff);
    }
    // TODO: drop after one release — transitional error to surface the rename.
    if target == "ceo" {
        anyhow::bail!("ceo has been renamed to chief-of-staff");
    }
    if target == "telegram" {
        return Ok(SendTarget::Telegram);
    }

    for (prefix_str, prefix) in [
        ("researcher:", AssistantPrefix::Researcher),
        ("reviewer:", AssistantPrefix::Reviewer),
    ] {
        if let Some(rest) = target.strip_prefix(prefix_str) {
            let pos = rest.find("--").ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid {kind} id '{rest}': expected '{kind}:<project>--<name>'",
                    kind = prefix.as_str()
                )
            })?;
            let project = &rest[..pos];
            let name = &rest[pos + 2..];
            if project.is_empty() || name.is_empty() {
                anyhow::bail!(
                    "invalid {kind} id '{rest}': expected '{kind}:<project>--<name>'",
                    kind = prefix.as_str()
                );
            }
            let dir = config.assistant_dir(project, name);
            if !dir.exists() {
                anyhow::bail!(
                    "unknown {kind} '{rest}' — no assistant found at {}",
                    dir.display(),
                    kind = prefix.as_str()
                );
            }
            return Ok(SendTarget::Assistant {
                project: project.to_string(),
                name: name.to_string(),
                prefix,
            });
        }
    }

    if let Some(task_id) = target.strip_prefix("task:") {
        if task_id.is_empty() {
            anyhow::bail!("invalid task id '': expected 'task:<repo>--<branch>'");
        }
        let dir = config.tasks_dir.join(task_id);
        if !dir.exists() {
            anyhow::bail!(
                "unknown task '{task_id}' — no task found at {}",
                dir.display()
            );
        }
        return Ok(SendTarget::Task(task_id.to_string()));
    }

    if target.contains(':') {
        anyhow::bail!("unknown target '{target}'\n{VALID_TARGETS_HINT}");
    }

    let dir = config.project_dir(target);
    if !dir.exists() {
        anyhow::bail!(
            "unknown project '{target}' — no project found at {}",
            dir.display()
        );
    }
    Ok(SendTarget::Project(target.to_string()))
}

/// Send a message to an agent's inbox.
pub fn send_message(config: &Config, target: &str, from: &str, message: &str) -> Result<()> {
    let inbox_path = agent_inbox_path(config, target)?;

    tracing::info!(target = target, from = from, "sending message");
    inbox::append_message(&inbox_path, from, message)?;
    Ok(())
}

/// Shorthand reference to an agent for UI rendering (e.g. Telegram button lists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    /// Agent id in send-message form: `"chief-of-staff"`, `"<project>"`, or `"researcher:<project>--<name>"`.
    pub id: String,
    /// Short human label (e.g. `"CoS"`, `"PM:foo"`, `"R:bar"`).
    pub label: String,
}

/// Resolve an agent id to its inbox path by routing through `parse_send_target`.
/// For the `telegram` pseudo-agent, returns the outbox path.
pub fn agent_inbox_path(config: &Config, id: &str) -> Result<PathBuf> {
    let parsed = parse_send_target(config, id)?;
    Ok(match parsed {
        SendTarget::ChiefOfStaff => config.chief_of_staff_inbox(),
        SendTarget::Telegram => config.telegram_outbox(),
        SendTarget::Project(name) => config.project_inbox(&name),
        SendTarget::Assistant { project, name, .. } => config.assistant_inbox(&project, &name),
        SendTarget::Task(task_id) => config.task_inbox(&task_id),
    })
}

/// True if `id` resolves to an agent that exists on disk.
pub fn agent_exists(config: &Config, id: &str) -> bool {
    parse_send_target(config, id).is_ok()
}

fn agent_ref_for(id: String) -> AgentRef {
    let label = if id == "chief-of-staff" {
        "CoS".to_string()
    } else if let Some(rest) = id.strip_prefix("researcher:") {
        let name = rest.rsplit("--").next().unwrap_or(rest);
        format!("R:{name}")
    } else if let Some(rest) = id.strip_prefix("reviewer:") {
        let name = rest.rsplit("--").next().unwrap_or(rest);
        format!("Rv:{name}")
    } else {
        format!("PM:{id}")
    };
    AgentRef { id, label }
}

/// Send-message id prefix for an assistant, derived from its kind.
fn assistant_send_id(a: &Assistant) -> String {
    let prefix = match a.meta.kind {
        AssistantKind::Researcher { .. } => "researcher",
        AssistantKind::Reviewer { .. } => "reviewer",
    };
    format!("{prefix}:{}--{}", a.meta.project, a.meta.name)
}

/// Agents reachable from `current` via a Telegram `/ls` switch list.
///
/// - From `"chief-of-staff"`: all PMs (sorted by name) plus all CoS-level
///   assistants (researchers and reviewers in `project == "chief-of-staff"`)
///   with `status == Running`.
/// - From `"<project>"`: that project's assistants (`Running` only) plus
///   `"chief-of-staff"`.
/// - From `"researcher:<proj>--<name>"` or `"reviewer:<proj>--<name>"`: its
///   parent (`"chief-of-staff"` if `proj == "chief-of-staff"`, otherwise the
///   project) plus `"chief-of-staff"` — de-duplicated by id.
pub fn relative_agent_list(config: &Config, current: &str) -> Vec<AgentRef> {
    let mut ids: Vec<String> = Vec::new();

    match current {
        "chief-of-staff" => {
            if let Ok(projects) = Project::list_all(config) {
                for p in projects {
                    ids.push(p.meta.name);
                }
            }
            if let Ok(assistants) = Assistant::list_for_project(config, "chief-of-staff") {
                for a in assistants {
                    if a.meta.status == AssistantStatus::Running {
                        ids.push(assistant_send_id(&a));
                    }
                }
            }
        }
        other => {
            let assistant_prefix = ["researcher:", "reviewer:"]
                .iter()
                .find_map(|p| other.strip_prefix(p));
            if let Some(rest) = assistant_prefix {
                let pos = rest.find("--");
                if let Some(pos) = pos {
                    let project = &rest[..pos];
                    let parent_id = if project == "chief-of-staff" {
                        "chief-of-staff".to_string()
                    } else {
                        project.to_string()
                    };
                    ids.push(parent_id);
                    ids.push("chief-of-staff".to_string());
                }
            } else {
                // Project-scoped view.
                if let Ok(assistants) = Assistant::list_for_project(config, other) {
                    for a in assistants {
                        if a.meta.status == AssistantStatus::Running {
                            ids.push(assistant_send_id(&a));
                        }
                    }
                }
                ids.push("chief-of-staff".to_string());
            }
        }
    }

    // De-duplicate preserving order.
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));

    ids.into_iter().map(agent_ref_for).collect()
}

/// Read the persisted current Telegram agent. Falls back to
/// `"chief-of-staff"` on any of:
/// - file missing / empty / IO error (silent),
/// - file contents that no longer resolve to an existing agent (`tracing::warn`).
///
/// Never returns an error — always yields a usable agent id.
pub fn read_current_agent(config: &Config) -> String {
    let path = config.telegram_current_agent_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return "chief-of-staff".to_string(),
    };
    let value = raw.trim();
    if value.is_empty() {
        return "chief-of-staff".to_string();
    }
    if !agent_exists(config, value) {
        tracing::warn!(stored = %value, "telegram current-agent: stale, falling back to chief-of-staff");
        return "chief-of-staff".to_string();
    }
    value.to_string()
}

/// Persist the Telegram current agent id atomically (tmp file + rename).
pub fn write_current_agent(config: &Config, value: &str) -> Result<()> {
    let path = config.telegram_current_agent_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, value).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
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
            tracing::warn!(target = target, "handoff timed out after {}s", timeout_secs);
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

/// Respawn an agent (Chief of Staff or PM) with a fresh session.
/// If `force` is false and the session is running, performs a graceful handoff first.
pub fn respawn_agent(config: &Config, target: &str, force: bool, timeout_secs: u64) -> Result<()> {
    if target.starts_with("researcher:") {
        tracing::info!(target = target, "rejected researcher respawn attempt");
        bail!("Respawning researchers is not supported. Create a new researcher instead.");
    }

    // Parse target to determine agent type and resolve paths
    let (state_dir, session_name, inbox_path) = if target == "chief-of-staff" {
        (
            config.chief_of_staff_dir(),
            Config::chief_of_staff_tmux_session().to_string(),
            config.chief_of_staff_inbox(),
        )
    } else {
        // PM for a project
        (
            config.project_dir(target),
            Config::pm_tmux_session(target),
            config.project_inbox(target),
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

    // Wipe long-lived session handles so the new spawn is FRESH (not a
    // resume of the killed thread). respawn intentionally drops prior
    // context — the handoff content carries forward what matters. Also
    // wipes the per-agent `harness` stamp so the next spawn re-reads the
    // current global setting (a flipped global is intentionally picked up
    // here, but never silently mid-conversation).
    wipe_long_lived_session_handles(&state_dir);
    tracing::info!(
        target = target,
        target_kind = %config.harness_kind(),
        "respawn: re-reading global harness setting for fresh spawn"
    );

    // Start new session, forcing a fresh launch.
    if target == "chief-of-staff" {
        start_chief_of_staff_session(config, true)?;
    } else {
        start_pm_session(config, target, true)?;
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

/// Delete the long-lived session handles from `state_dir`. Idempotent —
/// missing files are ignored. Called by `respawn_agent` to force the
/// next spawn to be fresh; also exposed for tests so the pre/post state
/// can be asserted without invoking tmux.
///
/// Wipes `harness` too: respawn means "kill, fresh spawn with handoff
/// briefing", which is the natural place to pick up a flipped global
/// harness. The next spawn re-reads `config.harness_kind()` via
/// `read_or_stamp` and stamps the new value. Pi has no extra stamped
/// handle beyond these files; its private session data is not parsed as
/// control metadata.
pub fn wipe_long_lived_session_handles(state_dir: &Path) {
    for fname in ["session-id", "session-name", "launch-cwd", "harness"] {
        let _ = std::fs::remove_file(state_dir.join(fname));
    }
}

/// Test-only view of a `LongLivedLaunch`. Exposes just enough internal
/// shape for integration tests to verify resume vs. fresh decisions
/// without spawning tmux. Stable by construction — the underlying
/// struct's only consumers are inside this module.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct LongLivedLaunchForTest {
    pub mode: &'static str, // "auto" | "pin" | "resume"
    pub handle: Option<String>,
    pub session_name: String,
    pub cwd: PathBuf,
    pub session_dir: Option<PathBuf>,
    pub is_first_launch: bool,
}

/// Test-only entrypoint mirroring `prepare_long_lived_launch`. The final
/// argument is retained for compatibility with older tests and ignored.
/// See `LongLivedLaunchForTest`.
#[doc(hidden)]
pub fn prepare_long_lived_launch_for_test(
    state_dir: &Path,
    base_name: &str,
    cwd: &Path,
    kind: HarnessKind,
    force_fresh: bool,
    _codex_home_override: Option<&Path>,
) -> Result<LongLivedLaunchForTest> {
    let prep = prepare_long_lived_launch_inner(state_dir, base_name, cwd, kind, force_fresh)?;
    let (mode, handle) = match &prep.mode {
        LaunchMode::Auto => ("auto", None),
        LaunchMode::Pin(h) => ("pin", Some(h.clone())),
        LaunchMode::Resume(h) => ("resume", Some(h.clone())),
    };
    Ok(LongLivedLaunchForTest {
        mode,
        handle,
        session_name: prep.session_name,
        cwd: prep.cwd,
        session_dir: prep.session_dir,
        is_first_launch: prep.is_first_launch,
    })
}

/// Result of preparing a long-lived agent launch — what to feed into
/// `Harness::build_session_command` and whether to run the post-launch
/// registration step (`/rename` for codex, `/name` for pi; no-op for
/// claude/goose).
struct LongLivedLaunch {
    /// Mode + the owned UUID/name backing the borrowed `SessionKey` returned
    /// by `session_key`. The handle lives inside the variant, so a `Pin` /
    /// `Resume` without a handle is unrepresentable.
    mode: LaunchMode,
    /// Working directory to actually launch in. Equals the stamped
    /// `<state_dir>/launch-cwd` when codex/goose/pi are resuming; otherwise the
    /// caller-supplied cwd.
    cwd: PathBuf,
    /// Private session dir for harnesses that require one. Pi resumes with
    /// `--continue` inside this directory; other harnesses leave it unset.
    session_dir: Option<PathBuf>,
    /// Stamped unique generation name. Passed as the harness session name
    /// for fresh launches, as the resume key for codex/goose, and as the
    /// human-visible `/name` value for pi.
    session_name: String,
    /// First time we've launched this long-lived agent (or `force_fresh`
    /// was set). Gates whether we run `register_long_lived_session`.
    is_first_launch: bool,
}

#[derive(Debug, Clone)]
enum LaunchMode {
    Auto,
    Pin(String),
    Resume(String),
}

impl LongLivedLaunch {
    fn session_key(&self) -> SessionKey<'_> {
        match &self.mode {
            LaunchMode::Auto => SessionKey::Auto,
            LaunchMode::Pin(h) => SessionKey::Pin(h),
            LaunchMode::Resume(h) => SessionKey::Resume(h),
        }
    }
}

/// Decide between fresh-launch and resume for a long-lived agent.
///
/// Claude path: keyed off `<state_dir>/session-id` (a UUID). On first
/// launch we mint one and write it; on subsequent launches we read it
/// back and use `Resume(uuid)`.
///
/// Codex/goose/pi path: keyed off `<state_dir>/session-name`. On first launch
/// we mint a unique generation name and stamp `<state_dir>/launch-cwd`;
/// on subsequent launches we resume that exact stamped name from the
/// stamped cwd. Pi also receives a private `<state_dir>/pi-sessions` dir
/// and resumes via `--continue`.
///
/// `force_fresh` short-circuits all paths: caller (respawn_agent) has
/// already wiped the handles; we mint a fresh generation.
fn prepare_long_lived_launch(
    state_dir: &Path,
    base_name: &str,
    cwd: &Path,
    kind: HarnessKind,
    force_fresh: bool,
) -> Result<LongLivedLaunch> {
    prepare_long_lived_launch_inner(state_dir, base_name, cwd, kind, force_fresh)
}

fn prepare_long_lived_launch_inner(
    state_dir: &Path,
    base_name: &str,
    cwd: &Path,
    kind: HarnessKind,
    force_fresh: bool,
) -> Result<LongLivedLaunch> {
    std::fs::create_dir_all(state_dir).context("failed to create agent state dir")?;
    let (session_name, fresh_session_name) =
        read_or_create_session_name(state_dir, base_name, force_fresh)?;

    match kind {
        HarnessKind::Claude => {
            let id_path = state_dir.join("session-id");
            if !force_fresh {
                if let Ok(raw) = std::fs::read_to_string(&id_path) {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        return Ok(LongLivedLaunch {
                            mode: LaunchMode::Resume(trimmed.to_string()),
                            cwd: cwd.to_path_buf(),
                            session_dir: None,
                            session_name,
                            is_first_launch: false,
                        });
                    }
                }
            }
            // Mint and stamp a fresh UUID.
            let uuid = uuid::Uuid::new_v4().to_string();
            std::fs::write(&id_path, &uuid)
                .with_context(|| format!("failed to write {}", id_path.display()))?;
            Ok(LongLivedLaunch {
                mode: LaunchMode::Pin(uuid),
                cwd: cwd.to_path_buf(),
                session_dir: None,
                session_name,
                is_first_launch: true,
            })
        }
        HarnessKind::Codex | HarnessKind::Goose | HarnessKind::Pi => {
            let cwd_path = Config::launch_cwd_path(state_dir);
            let session_dir =
                prepare_session_dir_for_harness(kind, state_dir, &session_name, true)?;

            if !force_fresh && !fresh_session_name {
                // Prefer the stamped launch-cwd if it still exists;
                // otherwise fall back to the freshly-resolved cwd.
                let resume_cwd = std::fs::read_to_string(&cwd_path)
                    .ok()
                    .map(|s| PathBuf::from(s.trim()))
                    .filter(|p| p.exists())
                    .unwrap_or_else(|| cwd.to_path_buf());
                Ok(LongLivedLaunch {
                    mode: LaunchMode::Resume(session_name.clone()),
                    cwd: resume_cwd,
                    session_dir,
                    session_name,
                    is_first_launch: false,
                })
            } else {
                // Idempotent stamp so future launches resume in the same dir.
                let stamp_value = cwd.to_string_lossy().to_string();
                if std::fs::read_to_string(&cwd_path)
                    .map(|s| s.trim() != stamp_value)
                    .unwrap_or(true)
                {
                    let _ = std::fs::write(&cwd_path, &stamp_value);
                }
                Ok(LongLivedLaunch {
                    mode: LaunchMode::Auto,
                    cwd: cwd.to_path_buf(),
                    session_dir,
                    session_name,
                    is_first_launch: true,
                })
            }
        }
    }
}

fn read_or_create_session_name(
    state_dir: &Path,
    base_name: &str,
    force_fresh: bool,
) -> Result<(String, bool)> {
    let path = state_dir.join("session-name");
    if !force_fresh {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Ok((trimmed.to_string(), false));
            }
        }
    }

    let session_name = unique_session_name(base_name);
    std::fs::write(&path, &session_name)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok((session_name, true))
}

fn unique_session_name(base_name: &str) -> String {
    let base = sanitize_session_name_base(base_name);
    let timestamp = chrono::Utc::now().format("%y%m%d-%H%M%S");
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!("{base}-{timestamp}-{}", &random[..8])
}

fn sanitize_session_name_base(base_name: &str) -> String {
    let mut out = String::with_capacity(base_name.len());
    let mut last_was_dash = false;
    for ch in base_name.chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            last_was_dash = false;
            ch
        } else if !last_was_dash {
            last_was_dash = true;
            '-'
        } else {
            continue;
        };
        out.push(next);
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "agman-agent".to_string()
    } else {
        trimmed.to_string()
    }
}

fn prepare_identity_file_for_harness(
    kind: HarnessKind,
    state_dir: &Path,
    session_name: &str,
    identity: &str,
    rewrite: bool,
) -> Result<Option<PathBuf>> {
    let path = match kind {
        HarnessKind::Goose => harness::goose::identity_file_path(state_dir, session_name),
        HarnessKind::Pi => harness::pi::identity_file_path(state_dir, session_name),
        _ => return Ok(None),
    };
    let should_write = match kind {
        HarnessKind::Pi => true,
        _ => rewrite || !path.exists(),
    };
    if should_write {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, identity)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(Some(path))
}

fn prepare_pi_session_dir(
    state_dir: &Path,
    session_name: &str,
    long_lived: bool,
) -> Result<PathBuf> {
    let path = if long_lived {
        harness::pi::long_lived_session_dir(state_dir)
    } else {
        harness::pi::task_session_dir(state_dir, session_name)
    };
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

fn prepare_session_dir_for_harness(
    kind: HarnessKind,
    state_dir: &Path,
    session_name: &str,
    long_lived: bool,
) -> Result<Option<PathBuf>> {
    if kind == HarnessKind::Pi {
        Ok(Some(prepare_pi_session_dir(
            state_dir,
            session_name,
            long_lived,
        )?))
    } else {
        Ok(None)
    }
}

#[doc(hidden)]
pub fn prepare_identity_file_for_harness_for_test(
    kind: HarnessKind,
    state_dir: &Path,
    session_name: &str,
    identity: &str,
    rewrite: bool,
) -> Result<Option<PathBuf>> {
    prepare_identity_file_for_harness(kind, state_dir, session_name, identity, rewrite)
}

/// Start the Chief of Staff agent session.
///
/// When `force_fresh` is true (e.g. from `respawn_agent`), any stamped
/// session handles in the Chief of Staff state dir are ignored and the
/// launch is treated as first-launch. Callers pass `false` for normal
/// "ensure-running" semantics.
pub fn start_chief_of_staff_session(config: &Config, force_fresh: bool) -> Result<()> {
    let cos_dir = config.chief_of_staff_dir();
    std::fs::create_dir_all(&cos_dir).context("failed to create Chief of Staff directory")?;

    let kind = harness::read_or_stamp(&cos_dir, config.harness_kind())?;
    let harness = kind.select();

    let (token, chat_id) = load_telegram_config(config);
    let telegram_enabled = token.as_deref().is_some_and(|t| !t.is_empty())
        && chat_id.as_deref().is_some_and(|c| !c.is_empty());
    let prompt = build_chief_of_staff_prompt(telegram_enabled);

    let session_name = Config::chief_of_staff_tmux_session();
    let agent_name = "agman-chief-of-staff".to_string();
    tracing::info!(session = session_name, telegram_enabled, harness = %kind, force_fresh, "starting Chief of Staff session");

    let prep = prepare_long_lived_launch(&cos_dir, &agent_name, &cos_dir, kind, force_fresh)?;
    let identity_file = prepare_identity_file_for_harness(
        kind,
        &cos_dir,
        &prep.session_name,
        &prompt,
        prep.is_first_launch,
    )?;
    // Pre-stamp workspace trust so the harness's first-launch trust dialog
    // doesn't block. Idempotent and cheap; failure is fatal because the
    // dialog is unrecoverable (the agent never reaches a usable state).
    harness
        .ensure_workspace_trusted(&prep.cwd)
        .with_context(|| {
            format!(
                "failed to pre-stamp workspace trust for Chief of Staff at {}",
                prep.cwd.display()
            )
        })?;
    let cmd = harness.build_session_command(&LaunchContext {
        identity: &prompt,
        name: &prep.session_name,
        identity_file: identity_file.as_deref(),
        session_dir: prep.session_dir.as_deref(),
        cwd: &prep.cwd,
        no_alt_screen: matches!(kind, HarnessKind::Codex),
        session_key: prep.session_key(),
    });

    let already_existed = Tmux::session_exists(session_name);
    Tmux::create_agent_session(session_name, &cmd, Some(&prep.cwd))?;

    if !already_existed && (prep.is_first_launch || kind == HarnessKind::Pi) {
        register_long_lived_session(harness.as_ref(), session_name, &prep.session_name, kind);
    }
    Ok(())
}

/// Wait for a long-lived agent session to be ready (foreground != shell)
/// and run the harness's post-launch registration. Best-effort — failures
/// log a warning but don't propagate (the session is still usable).
fn register_long_lived_session(
    harness: &dyn crate::harness::Harness,
    session: &str,
    name: &str,
    kind: HarnessKind,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok((true, _)) = Tmux::is_session_ready(session) {
            let harness_home = harness::harness_home(kind);
            if let Err(e) = harness.register_session_name(&RegisterContext {
                session,
                window: None,
                name,
                harness_home: &harness_home,
            }) {
                tracing::warn!(session, error = %e, "register_session_name failed");
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    tracing::warn!(
        session,
        "agent did not become foreground within 10s; skipping name registration"
    );
}

/// Open a Chief of Staff chat as a tmux popup overlaid on the current pane.
/// Ensures the persistent Chief of Staff session is running, then attaches a
/// popup to it.
///
/// Returns the spawned popup `Child` so the caller can poll it and keep the
/// main event loop ticking while the popup is open.
pub fn open_chief_of_staff_popup(config: &Config) -> Result<std::process::Child> {
    start_chief_of_staff_session(config, false)?;
    tracing::info!("opening Chief of Staff popup");
    Tmux::popup_attach(Config::chief_of_staff_tmux_session())
}

/// Start a PM agent session for a project. See `start_chief_of_staff_session` for the
/// `force_fresh` semantics.
pub fn start_pm_session(config: &Config, project_name: &str, force_fresh: bool) -> Result<()> {
    let project_dir = config.project_dir(project_name);
    if !project_dir.exists() {
        anyhow::bail!("project '{}' does not exist", project_name);
    }

    let kind = harness::read_or_stamp(&project_dir, config.harness_kind())?;
    let harness = kind.select();

    let (token, chat_id) = load_telegram_config(config);
    let telegram_enabled = token.as_deref().is_some_and(|t| !t.is_empty())
        && chat_id.as_deref().is_some_and(|c| !c.is_empty());
    let prompt = build_pm_prompt(telegram_enabled, project_name);
    let session_name = Config::pm_tmux_session(project_name);
    let agent_name = format!("agman-pm-{project_name}");
    tracing::info!(session = &session_name, project = project_name, telegram_enabled, harness = %kind, force_fresh, "starting PM session");

    let prep =
        prepare_long_lived_launch(&project_dir, &agent_name, &project_dir, kind, force_fresh)?;
    let identity_file = prepare_identity_file_for_harness(
        kind,
        &project_dir,
        &prep.session_name,
        &prompt,
        prep.is_first_launch,
    )?;
    harness
        .ensure_workspace_trusted(&prep.cwd)
        .with_context(|| {
            format!(
                "failed to pre-stamp workspace trust for PM '{}' at {}",
                project_name,
                prep.cwd.display()
            )
        })?;
    let cmd = harness.build_session_command(&LaunchContext {
        identity: &prompt,
        name: &prep.session_name,
        identity_file: identity_file.as_deref(),
        session_dir: prep.session_dir.as_deref(),
        cwd: &prep.cwd,
        no_alt_screen: matches!(kind, HarnessKind::Codex),
        session_key: prep.session_key(),
    });

    let already_existed = Tmux::session_exists(&session_name);
    Tmux::create_agent_session(&session_name, &cmd, Some(&prep.cwd))?;
    if !already_existed && (prep.is_first_launch || kind == HarnessKind::Pi) {
        register_long_lived_session(harness.as_ref(), &session_name, &prep.session_name, kind);
    }
    Ok(())
}

/// Open a PM chat as a tmux popup overlaid on the current pane.
/// Ensures the persistent PM session is running, then attaches a popup to it.
///
/// Returns the spawned popup `Child` so the caller can poll it and keep the
/// main event loop ticking while the popup is open.
pub fn open_pm_popup(config: &Config, project_name: &str) -> Result<std::process::Child> {
    start_pm_session(config, project_name, false)?;
    let session_name = Config::pm_tmux_session(project_name);
    tracing::info!(project = project_name, "opening PM popup");
    Tmux::popup_attach(&session_name)
}

/// Check if an agent's tmux session is running.
pub fn agent_session_running(session_name: &str) -> bool {
    Tmux::session_exists(session_name)
}

// ---------------------------------------------------------------------------
// Assistant management (Researcher + Reviewer)
// ---------------------------------------------------------------------------

/// Tmux session name for an assistant, dispatched on kind. Researchers keep
/// the legacy `agman-researcher-…` naming so existing sessions resume after
/// the assistant rename; reviewers use `agman-reviewer-…`.
fn assistant_tmux_session(meta: &crate::assistant::AssistantMeta) -> String {
    match meta.kind {
        AssistantKind::Researcher { .. } => {
            Config::researcher_tmux_session(&meta.project, &meta.name)
        }
        AssistantKind::Reviewer { .. } => Config::reviewer_tmux_session(&meta.project, &meta.name),
    }
}

/// Send-message id for an assistant (e.g. `"researcher:<proj>--<name>"`),
/// dispatched on kind.
fn assistant_send_target_id(meta: &crate::assistant::AssistantMeta) -> String {
    let prefix = match meta.kind {
        AssistantKind::Researcher { .. } => "researcher",
        AssistantKind::Reviewer { .. } => "reviewer",
    };
    format!("{prefix}:{}--{}", meta.project, meta.name)
}

/// Create a new researcher for a project. Researcher kind is the legacy
/// behavior; the assistant abstraction is what's new.
pub fn create_researcher(
    config: &Config,
    project: &str,
    name: &str,
    description: &str,
    repo: Option<String>,
    branch: Option<String>,
    task_id: Option<String>,
) -> Result<Assistant> {
    let kind = AssistantKind::Researcher {
        repo,
        branch,
        task_id,
    };
    create_assistant(config, project, name, description, kind)
}

/// Resolved (`(repo, branch)`, worktree path, `agman_created`) for a reviewer
/// before persisting the assistant. Built by `resolve_reviewer_worktrees`.
type ReviewerEntries = Vec<ReviewerWorktree>;

/// Spec for a reviewer at create time: which `(repo, branch)` pairs to scope
/// it to. The `parent_dir` knob mirrors the task code path — non-`None` means
/// repos live outside `config.repos_dir`. Used only by tests today; the CLI
/// always passes `None`.
#[derive(Debug, Clone)]
pub struct ReviewerSpec {
    pub branches: Vec<(String, String)>,
    pub parent_dir: Option<PathBuf>,
}

/// Create a new reviewer assistant. For each `(repo, branch)` pair the
/// resolution rules are:
/// 1. If a worktree already exists for that branch → use it as-is
///    (`agman_created = false`). No fetch, no writes.
/// 2. Else if a *local* branch exists without a worktree → bail loudly.
///    The user must either put the branch in a worktree first or delete the
///    local branch so we can fetch a clean copy from origin.
/// 3. Else → fetch origin, verify `origin/<branch>`, create a fresh worktree
///    at the standard path (`agman_created = true`).
///
/// The bail in step 2 happens **before** any fetch or worktree-add, so a
/// multi-branch reviewer failing on branch 3 of 5 doesn't litter state.
pub fn create_reviewer(
    config: &Config,
    project: &str,
    name: &str,
    description: &str,
    spec: ReviewerSpec,
) -> Result<Assistant> {
    if spec.branches.is_empty() {
        anyhow::bail!("reviewer requires at least one --branch <repo>:<branch>");
    }
    let worktrees = resolve_reviewer_worktrees(config, &spec)?;
    let kind = AssistantKind::Reviewer { worktrees };
    create_assistant(config, project, name, description, kind)
}

/// Walk the `(repo, branch)` list applying the three-step decision tree.
/// Returns the resolved `ReviewerWorktree` entries on success. On failure,
/// returns the first error encountered without creating any worktrees that
/// were resolved by step 3 in earlier iterations — callers should treat the
/// reviewer as not-created.
fn resolve_reviewer_worktrees(config: &Config, spec: &ReviewerSpec) -> Result<ReviewerEntries> {
    // Two-phase to honour the "no littering" rule: phase 1 classifies every
    // entry without side effects, bailing on the local-branch-no-worktree
    // case; phase 2 actually creates worktrees from origin for entries that
    // need one.
    enum Plan {
        Existing { path: PathBuf },
        FromOrigin,
    }
    let parent_dir = spec.parent_dir.as_deref();
    let mut planned: Vec<(String, String, Plan)> = Vec::with_capacity(spec.branches.len());
    for (repo, branch) in &spec.branches {
        let repo_path = config.repo_path_for(parent_dir, repo);
        if !repo_path.exists() {
            anyhow::bail!(
                "repository '{}' does not exist at {}",
                repo,
                repo_path.display()
            );
        }
        if let Some(path) = Git::find_worktree_for_branch(&repo_path, branch)? {
            planned.push((repo.clone(), branch.clone(), Plan::Existing { path }));
            continue;
        }
        if Git::local_branch_exists(&repo_path, branch) {
            anyhow::bail!(
                "branch '{}' exists locally in {} but is not checked out in a worktree. \
                 Either put it in a worktree first or delete the local branch so the \
                 reviewer can fetch a clean copy from origin.",
                branch,
                repo
            );
        }
        planned.push((repo.clone(), branch.clone(), Plan::FromOrigin));
    }

    let mut entries = Vec::with_capacity(planned.len());
    for (repo, branch, plan) in planned {
        match plan {
            Plan::Existing { path } => {
                entries.push(ReviewerWorktree {
                    repo,
                    branch,
                    path,
                    agman_created: false,
                });
            }
            Plan::FromOrigin => {
                let path = Git::create_worktree_from_origin(config, &repo, &branch, parent_dir)?;
                entries.push(ReviewerWorktree {
                    repo,
                    branch,
                    path,
                    agman_created: true,
                });
            }
        }
    }
    Ok(entries)
}

/// Shared assistant-creation backbone. Validates the project, persists the
/// meta, and queues the description into the assistant's inbox so the TUI
/// poller delivers it once the harness is ready.
fn create_assistant(
    config: &Config,
    project: &str,
    name: &str,
    description: &str,
    kind: AssistantKind,
) -> Result<Assistant> {
    if project != "chief-of-staff" {
        let project_dir = config.project_dir(project);
        if !project_dir.exists() {
            anyhow::bail!("project '{}' does not exist", project);
        }
    }

    tracing::info!(
        project = project,
        name = name,
        kind = match kind {
            AssistantKind::Researcher { .. } => "researcher",
            AssistantKind::Reviewer { .. } => "reviewer",
        },
        "creating assistant"
    );
    let assistant = Assistant::create(config, project, name, description, kind)?;

    // Queue the description as the first inbox message so the TUI poller
    // delivers it to the tmux session once the harness is ready.
    if !description.is_empty() {
        let inbox_path = config.assistant_inbox(project, name);
        crate::inbox::append_message(&inbox_path, "user", description)?;
        tracing::debug!(
            project = project,
            name = name,
            "queued assistant description to inbox"
        );
    }

    Ok(assistant)
}

/// Start an assistant's tmux session under the configured harness.
/// Dispatches on kind for prompt template and working-directory resolution;
/// everything else is kind-agnostic. See `start_chief_of_staff_session` for
/// the `force_fresh` semantics.
pub fn start_assistant_session(
    config: &Config,
    project: &str,
    name: &str,
    force_fresh: bool,
) -> Result<()> {
    let dir = config.assistant_dir(project, name);
    let assistant = Assistant::load(dir.clone())?;

    let kind = harness::read_or_stamp(&dir, config.harness_kind())?;
    let harness = kind.select();

    let (token, chat_id) = load_telegram_config(config);
    let telegram_enabled = token.as_deref().is_some_and(|t| !t.is_empty())
        && chat_id.as_deref().is_some_and(|c| !c.is_empty());

    // Kind-dispatched prompt + working-dir resolution.
    let (prompt, work_dir, agent_base, session_name) = match &assistant.meta.kind {
        AssistantKind::Researcher { .. } => {
            let prompt = build_researcher_prompt(telegram_enabled, project, name);
            let work_dir = resolve_researcher_work_dir(config, &assistant);
            (
                prompt,
                work_dir,
                format!("agman-r-{project}--{name}"),
                Config::researcher_tmux_session(project, name),
            )
        }
        AssistantKind::Reviewer { worktrees } => {
            let prompt = build_reviewer_prompt(telegram_enabled, project, name, worktrees);
            let cwd = worktrees.first().map(|w| w.path.clone());
            (
                prompt,
                cwd,
                format!("agman-rv-{project}--{name}"),
                Config::reviewer_tmux_session(project, name),
            )
        }
    };

    tracing::info!(
        session = &session_name,
        project = project,
        name = name,
        telegram_enabled,
        harness = %kind,
        force_fresh,
        kind = if assistant.is_researcher() { "researcher" } else { "reviewer" },
        "starting assistant session"
    );

    let cwd = work_dir.as_deref().unwrap_or(&dir);
    let prep = prepare_long_lived_launch(&dir, &agent_base, cwd, kind, force_fresh)?;
    let identity_file = prepare_identity_file_for_harness(
        kind,
        &dir,
        &prep.session_name,
        &prompt,
        prep.is_first_launch,
    )?;
    harness
        .ensure_workspace_trusted(&prep.cwd)
        .with_context(|| {
            format!(
                "failed to pre-stamp workspace trust for assistant '{}--{}' at {}",
                project,
                name,
                prep.cwd.display()
            )
        })?;
    let cmd = harness.build_session_command(&LaunchContext {
        identity: &prompt,
        name: &prep.session_name,
        identity_file: identity_file.as_deref(),
        session_dir: prep.session_dir.as_deref(),
        cwd: &prep.cwd,
        no_alt_screen: matches!(kind, HarnessKind::Codex),
        session_key: prep.session_key(),
    });

    let already_existed = Tmux::session_exists(&session_name);
    Tmux::create_agent_session(&session_name, &cmd, Some(&prep.cwd))?;
    if !already_existed && (prep.is_first_launch || kind == HarnessKind::Pi) {
        register_long_lived_session(harness.as_ref(), &session_name, &prep.session_name, kind);
    }

    Ok(())
}

/// Resolve the working directory for a Researcher assistant session.
/// Reviewers use the first worktree path directly; this helper is researcher-only.
fn resolve_researcher_work_dir(config: &Config, assistant: &Assistant) -> Option<PathBuf> {
    let AssistantKind::Researcher {
        repo,
        branch,
        task_id,
    } = &assistant.meta.kind
    else {
        return None;
    };

    // If task_id is set, try to resolve to the task's worktree path
    if let Some(task_id) = task_id {
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
    if let (Some(repo), Some(branch)) = (repo, branch) {
        let wt_dir = config
            .repos_dir
            .parent()?
            .join(format!("{repo}-wt"))
            .join(branch);
        if wt_dir.exists() {
            return Some(wt_dir);
        }
    }

    // If repo only, resolve to main repo dir
    if let Some(repo) = repo {
        let repo_dir = config.repos_dir.join(repo);
        if repo_dir.exists() {
            return Some(repo_dir);
        }
    }

    None
}

/// List assistants, optionally filtered by project and/or kind.
pub fn list_assistants(
    config: &Config,
    project: Option<&str>,
    kind: Option<AssistantKindLabel>,
) -> Result<Vec<Assistant>> {
    let mut all = match project {
        Some(p) => Assistant::list_for_project(config, p)?,
        None => Assistant::list_all(config)?,
    };
    if let Some(k) = kind {
        all.retain(|a| {
            matches!(
                (&a.meta.kind, k),
                (
                    AssistantKind::Researcher { .. },
                    AssistantKindLabel::Researcher
                ) | (AssistantKind::Reviewer { .. }, AssistantKindLabel::Reviewer)
            )
        });
    }
    Ok(all)
}

/// Backwards-compatible researcher list (filters to the Researcher kind).
pub fn list_researchers(config: &Config, project: Option<&str>) -> Result<Vec<Assistant>> {
    list_assistants(config, project, Some(AssistantKindLabel::Researcher))
}

/// Archive an assistant. Researchers: kill tmux + flip status. Reviewers:
/// the same plus per-worktree cleanup of any entries we created (`git
/// worktree remove --force` and `git branch -D` per `agman_created=true`
/// entry). Worktrees that pre-existed when the reviewer was created are left
/// alone — see plan for details.
pub fn archive_assistant(config: &Config, project: &str, name: &str) -> Result<()> {
    let dir = config.assistant_dir(project, name);
    let mut assistant = Assistant::load(dir)?;

    let session_name = assistant_tmux_session(&assistant.meta);
    if Tmux::session_exists(&session_name) {
        tracing::info!(session = &session_name, "killing assistant tmux session");
        Tmux::kill_session(&session_name)?;
    }

    assistant.meta.status = AssistantStatus::Archived;
    assistant.save_meta()?;

    if let AssistantKind::Reviewer { worktrees } = &assistant.meta.kind {
        for entry in worktrees {
            if !entry.agman_created {
                continue;
            }
            let repo_path = config.repo_path(&entry.repo);
            if let Err(e) = Git::remove_worktree(&repo_path, &entry.path) {
                tracing::warn!(repo = %entry.repo, branch = %entry.branch, error = %e, "failed to remove reviewer worktree");
            }
            if let Err(e) = Git::delete_branch(&repo_path, &entry.branch) {
                tracing::warn!(repo = %entry.repo, branch = %entry.branch, error = %e, "failed to delete reviewer branch");
            }
        }
    }

    tracing::info!(project = project, name = name, "assistant archived");
    Ok(())
}

/// Backwards-compatible researcher archive — delegates to `archive_assistant`.
pub fn archive_researcher(config: &Config, project: &str, name: &str) -> Result<()> {
    archive_assistant(config, project, name)
}

/// Resume an archived assistant: start a new tmux session and flip status to
/// Running. `start_assistant_session` will pick up any stamped session-id
/// (claude) or session-name (codex/goose/pi) and resume the underlying
/// conversation if available; archive does not delete those handles.
pub fn resume_assistant(config: &Config, project: &str, name: &str) -> Result<()> {
    start_assistant_session(config, project, name, false)?;

    let dir = config.assistant_dir(project, name);
    let mut assistant = Assistant::load(dir)?;
    assistant.meta.status = AssistantStatus::Running;
    assistant.save_meta()?;

    tracing::info!(project = project, name = name, "assistant resumed");
    Ok(())
}

/// Backwards-compat alias used by callers that haven't migrated to the new
/// name yet. Identical to `start_assistant_session`.
pub fn start_researcher_session(
    config: &Config,
    project: &str,
    name: &str,
    force_fresh: bool,
) -> Result<()> {
    start_assistant_session(config, project, name, force_fresh)
}

/// Backwards-compat alias used by callers that haven't migrated to the new
/// name yet. Identical to `resume_assistant`.
pub fn resume_researcher(config: &Config, project: &str, name: &str) -> Result<()> {
    resume_assistant(config, project, name)
}

// ---------------------------------------------------------------------------
// Inbox polling target enumeration
// ---------------------------------------------------------------------------

/// A destination for inbox polling: identifies where to read undelivered messages
/// and which tmux session to deliver them to.
#[derive(Debug, Clone)]
pub struct InboxPollTarget {
    /// `"chief-of-staff"`, `"<project>"`, `"researcher:<project>--<name>"`,
    /// `"reviewer:<project>--<name>"`, or `"task:<id>"`.
    pub name: String,
    pub inbox_path: PathBuf,
    pub seq_path: PathBuf,
    pub session_name: String,
    /// Optional window within `session_name` where delivery should happen.
    /// `None` for single-window sessions (Chief of Staff/PM/assistant);
    /// `Some("agman")` for task sessions whose interactive harness lives in the
    /// `agman` window.
    pub window: Option<String>,
    /// Optional re-arm marker path. When present on disk, the poll worker
    /// drops its in-memory cold-start ready-buffer entry for this target and
    /// deletes the file, forcing the buffer to restart from zero on the next
    /// observed-ready tick. Currently set only for task targets (the
    /// supervisor touches `<task_dir>/.inbox-rearm` across kill→relaunch
    /// transitions). `None` for Chief of Staff/PM/assistant targets, whose
    /// harness is never restarted under the supervisor.
    pub rearm_path: Option<PathBuf>,
}

/// Enumerate all inbox delivery targets from disk.
///
/// Reads projects and researchers directly from the filesystem so delivery does
/// not depend on which TUI view the user has visited. `session_exists` is a
/// predicate — production callers pass `Tmux::session_exists`; tests pass a
/// stub like `|_| true`.
pub fn collect_inbox_poll_targets(
    config: &Config,
    session_exists: impl Fn(&str) -> bool,
) -> Vec<InboxPollTarget> {
    let mut targets = Vec::new();

    // Chief of Staff target
    let cos_session = Config::chief_of_staff_tmux_session().to_string();
    if session_exists(&cos_session) {
        targets.push(InboxPollTarget {
            name: "chief-of-staff".to_string(),
            inbox_path: config.chief_of_staff_inbox(),
            seq_path: config.chief_of_staff_seq(),
            session_name: cos_session,
            window: None,
            rearm_path: None,
        });
    }

    // PM targets
    match Project::list_all(config) {
        Ok(projects) => {
            for p in projects {
                let session_name = Config::pm_tmux_session(&p.meta.name);
                if session_exists(&session_name) {
                    targets.push(InboxPollTarget {
                        name: p.meta.name.clone(),
                        inbox_path: config.project_inbox(&p.meta.name),
                        seq_path: config.project_seq(&p.meta.name),
                        session_name,
                        window: None,
                        rearm_path: None,
                    });
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "collect_inbox_poll_targets: failed to list projects, skipping PM targets");
        }
    }

    // Assistant targets (researchers + reviewers, single iteration over disk).
    match Assistant::list_all(config) {
        Ok(assistants) => {
            for a in assistants {
                if a.meta.status != AssistantStatus::Running {
                    continue;
                }
                let session_name = assistant_tmux_session(&a.meta);
                if session_exists(&session_name) {
                    targets.push(InboxPollTarget {
                        name: assistant_send_target_id(&a.meta),
                        inbox_path: config.assistant_inbox(&a.meta.project, &a.meta.name),
                        seq_path: config.assistant_seq(&a.meta.project, &a.meta.name),
                        session_name,
                        window: None,
                        rearm_path: None,
                    });
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "collect_inbox_poll_targets: failed to list assistants, skipping assistant targets");
        }
    }

    // Task targets. Tasks host their interactive harness in the `agman` window
    // of their session (single-repo: primary repo's session; multi-repo:
    // parent-dir session). `session_name` carries only the session; the
    // caller must deliver to the `agman` window specifically via the
    // window-aware tmux APIs.
    for task in Task::list_all(config) {
        if task.meta.status != TaskStatus::Running {
            continue;
        }
        let session_name = if task.meta.is_multi_repo() {
            Config::tmux_session_name(&task.meta.name, &task.meta.branch_name)
        } else if task.meta.has_repos() {
            task.meta.primary_repo().tmux_session.clone()
        } else {
            continue;
        };
        if !session_exists(&session_name) {
            continue;
        }
        let task_id = task.meta.task_id();
        targets.push(InboxPollTarget {
            name: format!("task:{task_id}"),
            inbox_path: config.task_inbox(&task_id),
            seq_path: config.task_inbox_seq(&task_id),
            session_name,
            window: Some(crate::supervisor::AGMAN_WINDOW.to_string()),
            rearm_path: Some(task.rearm_path()),
        });
    }

    targets
}

/// Filter a counts map to the subset whose count meets `threshold`.
///
/// Pulled out as a free function so we can unit-test the threshold logic
/// without spinning up an `App`.
pub fn stalled_targets_from_counts(
    counts: &std::collections::HashMap<String, u32>,
    threshold: u32,
) -> Vec<&str> {
    counts
        .iter()
        .filter(|(_, n)| **n >= threshold)
        .map(|(t, _)| t.as_str())
        .collect()
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
    let (repo, branch) =
        Config::parse_task_id(task_id).context(format!("invalid task ID: {}", task_id))?;
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
                        let agents: Vec<&str> = l.steps.iter().map(|s| s.agent.as_str()).collect();
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
            task.meta.flow_name,
            task.meta.flow_step + 1,
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
        out.push_str(&format!(
            "Queue: {} item{}\n",
            queue.len(),
            if queue.len() == 1 { "" } else { "s" }
        ));
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
                    let branch_str = branch
                        .as_deref()
                        .map(|b| format!(" ({})", b))
                        .unwrap_or_default();
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
    let (repo, branch) =
        Config::parse_task_id(task_id).context(format!("invalid task ID: {}", task_id))?;
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
    let (repo, branch) =
        Config::parse_task_id(task_id).context(format!("invalid task ID: {}", task_id))?;
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

/// Persist the chosen harness as the global default for newly-spawned agents.
pub fn save_harness(config: &Config, kind: HarnessKind) -> Result<()> {
    let mut cf = crate::config::load_config_file(&config.base_dir);
    cf.harness = Some(kind.as_str().to_string());
    crate::config::save_config_file(&config.base_dir, &cf)
}

// ---------------------------------------------------------------------------
// Default system prompts
// ---------------------------------------------------------------------------

const DEFAULT_CHIEF_OF_STAFF_PROMPT: &str = r#"You are the Chief of Staff (CoS) for agman. The user is the CEO and runs the show. You support the CEO by staying in the loop on every project, helping the CEO maintain a clear mental model of what's happening, and answering "where are we at?" / "what's blocked?" / "what should we move forward with?" questions on demand.

You have full agman command access. When the CEO directs you to do something — create a project, brief a PM, redirect work — you do it. But when nothing has been directed, your default stance is cautious: don't act unilaterally, don't invent strategy, don't push your own agenda.

## Information Intake

PMs send you summaries at natural stopping points: a task finished, a task blocked, a researcher reported back, a significant progress milestone. Read every summary, hold it in working memory, but do not respond unless the PM asked you a question or you need to nudge them on something the CEO has pre-authorized.

Always cross-reference your mental model against current ground truth before answering status questions. Never reply purely from memory — verify with:
- agman list-projects — overview of all projects with PM status and task counts
- agman project-status <name> — single-project deep view
- agman list-pm-tasks <project> — task list for a project
- agman task-status <task-id> — task detail
- agman task-log <task-id> --tail 100 — recent task log

## Authority

You CAN do anything the CEO directs you to do. Available write commands:
- agman create-project <name> --description "<label>" [--initial-message <text|@file|->]
- agman delete-project <name>
- agman create-pm-task <project> <repo> <task-name> --description "..." (rare — usually PMs do this)
- agman feedback <task-id> "..."
- agman send-message <target> --from chief-of-staff
- agman create-researcher <name> [--project <name>]
- agman archive-researcher <name> [--project <name>]
- agman respawn-agent <target> (rare)
- All project-template commands (list-templates, get-template, etc.)

You SHOULD use these commands only when:
1. The CEO has given you a direct instruction, OR
2. The CEO has pre-authorized a class of actions, OR
3. The action is genuinely obvious, low-risk, and reversible.

You MUST NOT:
- Create projects, kill projects, or invent strategy on your own initiative.
- Override CEO decisions.
- Act on ambiguous requests — escalate to the CEO instead.
- Push your own agenda when the CEO is silent.

When in doubt, ask. The CEO would rather wait 30 seconds for confirmation than have you do the wrong thing.

## Voice Priority

The CEO's direct messages always supersede your direction to PMs. If you have nudged a PM and the CEO contradicts you, immediately tell the PM that the CEO's direction takes over and stop pushing your version.

## Communication

Send to a PM:
cat <<'AGMAN_MSG' | agman send-message <project-name> --from chief-of-staff
<message content>
AGMAN_MSG

Send to the user via Telegram:
cat <<'AGMAN_MSG' | agman send-message telegram --from chief-of-staff
<message content>
AGMAN_MSG

When you receive a message from a PM, respond via send-message — your tmux session is invisible to them.

## Project Templates

When the CEO asks you to start a project that matches a template:
1. agman list-templates — see available templates
2. agman get-template <name> > /tmp/brief.md — copy to scratch file
3. Edit the scratch file to fit the instance
4. agman create-project <proj> --description "<label>" --initial-message @/tmp/brief.md

Never edit a template just to customize one project — modify the scratch file.

## Behavior Guidelines

- PMs own all process decisions. When briefing a PM, relay intent, scope, and context — then get out of their way. Do NOT tell PMs how to structure tasks, sequence work, or handle tracking artifacts. Only override when the CEO explicitly tells you to be prescriptive.
- When the CEO asks a question, answer it — do not treat it as an implicit instruction to take action.
- Escalate blockers to the CEO. Never sit on a blocker.

## Style

Concise. Bullets, not essays. The CEO uses you to avoid getting overwhelmed — long messages defeat the purpose. Status answers should fit on one screen.

## System Messages

Messages tagged [Message from system]: are automated agman notifications. Act on them autonomously per the message instructions. No reply needed.
"#;

fn build_chief_of_staff_prompt(telegram_enabled: bool) -> String {
    if !telegram_enabled {
        return DEFAULT_CHIEF_OF_STAFF_PROMPT.to_string();
    }

    let telegram_section = r#"

## Telegram

Telegram is connected and active.

The one rule you must never break: acknowledge first, work second, report third.

Messages tagged [Message from telegram] come from the CEO on their phone. They cannot see your tmux chat — the only way to respond is via Telegram.

Every [Message from telegram] triggers this exact sequence:
1. IMMEDIATELY acknowledge — before any investigation, planning, or delegation. Send a short ack via Telegram. Do not read files, do not call agman commands — just acknowledge.
2. Do the work.
3. Report back via Telegram with the result.

Send command:
cat <<'AGMAN_MSG' | agman send-message telegram --from chief-of-staff
<your reply>
AGMAN_MSG

Keep Telegram replies concise. The CEO sees [CoS] prepended to your replies.
"#;

    format!("{}{}", DEFAULT_CHIEF_OF_STAFF_PROMPT, telegram_section)
}

pub fn build_pm_prompt(telegram_enabled: bool, project_name: &str) -> String {
    let base = DEFAULT_PM_PROMPT_TEMPLATE.replace("{{PROJECT_NAME}}", project_name);
    if !telegram_enabled {
        return base;
    }

    let telegram_section = format!(
        r#"

## Telegram

Telegram is connected and the user can switch chats over to you directly. When that happens, messages tagged `[Message from telegram]` will appear in your tmux session.

**The one rule you must never break: acknowledge first, work second, report third.**

`[Message from telegram]` means the user is on their phone and **cannot** see your tmux session. The only way to reach them is via the Telegram send command. The user is staring at their phone waiting for any sign that you saw the message. Silence while you work is not acceptable.

Every `[Message from telegram]` triggers this exact sequence:

1. **IMMEDIATELY acknowledge** — Your very first action, before any investigation, task creation, or delegation, is to send a short acknowledgment via Telegram (e.g. "Got it, looking into this now" or "On it — will report back shortly"). Do this BEFORE running any other command. Do not read files, do not list tasks, do not think through the problem first — just acknowledge.
2. **Do the work** — Then proceed with whatever was requested (investigate, create tasks, brief researchers, etc.).
3. **Report back** — When the work is done (or you've hit a decision point), send a follow-up Telegram message with the result or outcome.

Send command:
```
cat <<'AGMAN_MSG' | agman send-message telegram --from {project_name}
<your reply>
AGMAN_MSG
```

Additional rules:
- Keep Telegram replies concise — this is a mobile chat, not a report.
- The user sees `[PM:{project_name}]` prepended to your replies, so they always know who is speaking.
- Never leave the user waiting in silence while you work. Acknowledge first, work second, report third.
"#
    );

    format!("{base}{telegram_section}")
}

pub fn build_researcher_prompt(
    telegram_enabled: bool,
    project_name: &str,
    researcher_name: &str,
) -> String {
    let template = if project_name == "chief-of-staff" {
        DEFAULT_CHIEF_OF_STAFF_RESEARCHER_PROMPT_TEMPLATE
    } else {
        DEFAULT_RESEARCHER_PROMPT_TEMPLATE
    };
    let base = template
        .replace("{{PROJECT_NAME}}", project_name)
        .replace("{{RESEARCHER_NAME}}", researcher_name);
    if !telegram_enabled {
        return base;
    }

    let telegram_section = format!(
        r#"

## Telegram

Telegram is connected and the user can switch chats over to you directly. When that happens, messages tagged `[Message from telegram]` will appear in your tmux session.

**The one rule you must never break: acknowledge first, work second, report third.**

`[Message from telegram]` means the user is on their phone and **cannot** see your tmux session. The only way to reach them is via the Telegram send command. The user is staring at their phone waiting for any sign that you saw the message. Silence while you research is not acceptable.

Every `[Message from telegram]` triggers this exact sequence:

1. **IMMEDIATELY acknowledge** — Your very first action, before any investigation or research, is to send a short acknowledgment via Telegram (e.g. "Got it, looking into this now" or "On it — will report back shortly"). Do this BEFORE running any other command. Do not read files, do not grep the codebase, do not think through the problem first — just acknowledge.
2. **Do the work** — Then proceed with the research or investigation.
3. **Report back** — When your research is done (or you've hit a decision point), send a follow-up Telegram message with your findings.

Send command:
```
cat <<'AGMAN_MSG' | agman send-message telegram --from "researcher:{project_name}--{researcher_name}"
<your reply>
AGMAN_MSG
```

Additional rules:
- Keep Telegram replies concise — this is a mobile chat, not a report.
- The user sees `[R:{researcher_name}]` prepended to your replies, so they always know who is speaking.
- Never leave the user waiting in silence while you work. Acknowledge first, work second, report third.
"#
    );

    format!("{base}{telegram_section}")
}

/// Build a reviewer assistant's system prompt. Pattern mirrors
/// `build_researcher_prompt` — same telegram opt-in, same heredoc style — but
/// the body is reviewer-specific: read-only worktree audits with the explicit
/// rules from the plan (no fetch, no writes, no GitHub posts).
pub fn build_reviewer_prompt(
    telegram_enabled: bool,
    project_name: &str,
    reviewer_name: &str,
    worktrees: &[ReviewerWorktree],
) -> String {
    let template = if project_name == "chief-of-staff" {
        DEFAULT_CHIEF_OF_STAFF_REVIEWER_PROMPT_TEMPLATE
    } else {
        DEFAULT_REVIEWER_PROMPT_TEMPLATE
    };
    let worktree_block = if worktrees.is_empty() {
        "(no worktrees configured)".to_string()
    } else {
        worktrees
            .iter()
            .map(|w| format!("- {}:{} → {}", w.repo, w.branch, w.path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let base = template
        .replace("{{PROJECT_NAME}}", project_name)
        .replace("{{REVIEWER_NAME}}", reviewer_name)
        .replace("{{WORKTREES}}", &worktree_block);
    if !telegram_enabled {
        return base;
    }

    let telegram_section = format!(
        r#"

## Telegram

Telegram is connected and the user can switch chats over to you directly. When that happens, messages tagged `[Message from telegram]` will appear in your tmux session.

**The one rule you must never break: acknowledge first, work second, report third.**

`[Message from telegram]` means the user is on their phone and **cannot** see your tmux session. The only way to reach them is via the Telegram send command. The user is staring at their phone waiting for any sign that you saw the message. Silence while you review is not acceptable.

Every `[Message from telegram]` triggers this exact sequence:

1. **IMMEDIATELY acknowledge** — Your very first action, before any review work, is to send a short acknowledgment via Telegram (e.g. "Got it, looking into this now" or "On it — will report back shortly"). Do this BEFORE running any other command.
2. **Do the work** — Then proceed with the review.
3. **Report back** — When the review is done (or you've hit a decision point), send a follow-up Telegram message with your findings.

Send command:
```
cat <<'AGMAN_MSG' | agman send-message telegram --from "reviewer:{project_name}--{reviewer_name}"
<your reply>
AGMAN_MSG
```

Additional rules:
- Keep Telegram replies concise — this is a mobile chat, not a report.
- The user sees `[Rv:{reviewer_name}]` prepended to your replies, so they always know who is speaking.
- Never leave the user waiting in silence while you work. Acknowledge first, work second, report third.
"#
    );

    format!("{base}{telegram_section}")
}

const DEFAULT_REVIEWER_PROMPT_TEMPLATE: &str = r#"You are a code reviewer assistant for project "{{PROJECT_NAME}}", named "{{REVIEWER_NAME}}".

Your job is to read code from the worktrees listed below — including uncommitted, staged, and unstaged changes — and deliver opinions back to the PM. You are stateless between questions but the session is long-lived: the PM may follow up with more questions on the same review.

## Worktrees

{{WORKTREES}}

The local filesystem is authoritative. Treat each worktree as the source of truth for what the branch currently looks like.

## Hard rules

- Do **not** fetch from origin. The user updates the worktree themselves and asks you to look again — never run `git fetch`, `git pull`, or any other network-touching git command.
- Do **not** write to disk. No new files, no edits, no commits, no notes-to-self.
- Do **not** post to GitHub. No `gh pr review`, no comments, no labels, no merges.
- Do **not** open a PR or interact with one. PR-URL → branch translation is the PM's job; you only see the worktrees above.
- Do **not** write a `REVIEW.md` or any artifact file.

## Communication

Messages from the PM appear in your tmux session tagged `[Message from {{PROJECT_NAME}}]:`. The PM **cannot** see your tmux session — you MUST reply using `agman send-message`. Never just type a response in tmux expecting the PM to see it.

Reply via:
```
cat <<'AGMAN_MSG' | agman send-message {{PROJECT_NAME}} --from "reviewer:{{PROJECT_NAME}}--{{REVIEWER_NAME}}"
<your findings>
AGMAN_MSG
```

Keep findings concise and specific — file paths, line numbers, and the actual issue. The PM will follow up with more questions if they need to dig deeper.
"#;

const DEFAULT_CHIEF_OF_STAFF_REVIEWER_PROMPT_TEMPLATE: &str = r#"You are a code reviewer assistant for the Chief of Staff, named "{{REVIEWER_NAME}}".

Your job is to read code from the worktrees listed below — including uncommitted, staged, and unstaged changes — and deliver opinions back to the Chief of Staff. You are stateless between questions but the session is long-lived: the CoS may follow up with more questions on the same review.

## Worktrees

{{WORKTREES}}

The local filesystem is authoritative. Treat each worktree as the source of truth for what the branch currently looks like.

## Hard rules

- Do **not** fetch from origin. Never run `git fetch`, `git pull`, or any other network-touching git command.
- Do **not** write to disk. No new files, no edits, no commits, no notes-to-self.
- Do **not** post to GitHub. No `gh pr review`, no comments, no labels, no merges.
- Do **not** open a PR or interact with one. PR-URL → branch translation is the CoS's job; you only see the worktrees above.
- Do **not** write a `REVIEW.md` or any artifact file.

## Communication

Messages from the Chief of Staff appear tagged `[Message from chief-of-staff]:`. The Chief of Staff cannot see your tmux session — you MUST reply using agman send-message.

Reply via:
```
cat <<'AGMAN_MSG' | agman send-message chief-of-staff --from "reviewer:chief-of-staff--{{REVIEWER_NAME}}"
<your findings>
AGMAN_MSG
```

Keep findings concise and specific — file paths, line numbers, and the actual issue. The Chief of Staff will follow up with more questions if they need to dig deeper.
"#;

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

const DEFAULT_CHIEF_OF_STAFF_RESEARCHER_PROMPT_TEMPLATE: &str = r#"You are a researcher for the Chief of Staff, named "{{RESEARCHER_NAME}}".

Your role is to explore, analyze, and answer questions. You are NOT here to make code changes — only to investigate and report findings.

Messages from the Chief of Staff appear tagged [Message from chief-of-staff]:. The Chief of Staff cannot see your tmux session — you MUST reply using agman send-message.

ALL findings and responses must go through send-message:
cat <<'AGMAN_MSG' | agman send-message chief-of-staff --from "researcher:chief-of-staff--{{RESEARCHER_NAME}}"
<your findings>
AGMAN_MSG

Keep reports concise and actionable. When you've completed your research, summarize key findings in a single message.
"#;

const DEFAULT_PM_PROMPT_TEMPLATE: &str = r#"You are the Project Manager (PM) for the "{{PROJECT_NAME}}" project in agman. You manage tasks to accomplish your project's goals.

## Your Role
- Receive goals from the CEO (the user, when they message you directly) or from the Chief of Staff (acting on the CEO's behalf), and break them into concrete tasks.
- Create and monitor tasks within your project.
- Send stopping-point summaries to the Chief of Staff so the CoS stays informed.
- Break goals into concrete, well-scoped tasks.

## Available Commands (use via Bash tool)

### Task Management
- agman create-pm-task {{PROJECT_NAME}} <repo> <task-name> --description "<description>"
- agman list-pm-tasks {{PROJECT_NAME}}
- agman task-status <task-id>
- agman task-log <task-id> --tail 100

### Communication
Report to the Chief of Staff:
cat <<'AGMAN_MSG' | agman send-message chief-of-staff --from {{PROJECT_NAME}}
<message content>
AGMAN_MSG

## Behavior Guidelines

- When given work, suggest a task plan to the requester and wait for confirmation before creating tasks.
- When the CEO asks a question, answer it — do not treat it as an implicit instruction to take action.
- If a task fails, analyze the logs and either retry or escalate.
- Never run long commands yourself — always spawn a task for implementation work.

### Stopping-Point Summaries

At every natural stopping point — task finished, task blocked, researcher reported back, significant progress — send a brief summary to the Chief of Staff:

cat <<'AGMAN_MSG' | agman send-message chief-of-staff --from {{PROJECT_NAME}}
<one-paragraph summary: what was done, where things are, why you stopped>
AGMAN_MSG

Do NOT skip these because "the CEO already knows" — they are for the CoS's mental model.

If the CEO is also waiting on you, respond to the CEO first, then send the CoS summary.

### Voice Priority

Direct CEO messages always take priority over CoS direction. If they conflict, follow the CEO and tell the CoS.

## Reactive Behavior

- Relay completions: When a task or researcher completes/fails/needs input, send a brief summary to the Chief of Staff with outcome details.
- Do NOT poll: Tasks and researchers notify you — wait for notifications.

## Message Routing

[Message from chief-of-staff]: — direction or status checks from the CoS. Reply briefly via send-message.
cat <<'AGMAN_MSG' | agman send-message chief-of-staff --from {{PROJECT_NAME}}
<your reply>
AGMAN_MSG

[Message from researcher:{{PROJECT_NAME}}--<name>]: — researcher reports. Reply via send-message.
cat <<'AGMAN_MSG' | agman send-message researcher:{{PROJECT_NAME}}--<researcher-name> --from {{PROJECT_NAME}}
<your reply>
AGMAN_MSG

Direct CEO input (no tag) — the CEO is in your tmux session. Respond inline. If CEO direction conflicts with CoS, the CEO wins.
"#;
