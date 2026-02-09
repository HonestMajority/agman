use anyhow::Result;
use std::path::PathBuf;

use crate::config::Config;
use crate::git::Git;
use crate::repo_stats::RepoStats;
use crate::task::{Task, TaskStatus};

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
    let repo_name = &task.meta.repo_name;
    let branch_name = &task.meta.branch_name;
    let worktree_path = &task.meta.worktree_path;

    match mode {
        DeleteMode::Everything => {
            let repo_path = config.repo_path(repo_name);
            let _ = Git::remove_worktree(&repo_path, worktree_path);
            let _ = Git::delete_branch(&repo_path, branch_name);
        }
        DeleteMode::TaskOnly => {
            let task_md_path = worktree_path.join("TASK.md");
            if task_md_path.exists() {
                let _ = std::fs::remove_file(&task_md_path);
            }
        }
    }

    // Delete task directory
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
    task.update_status(TaskStatus::Stopped)?;
    task.meta.current_agent = None;
    task.save_meta()?;
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
    task.update_status(TaskStatus::Running)?;
    Ok(())
}

/// Queue feedback on a running task.
///
/// Extracts the "running" branch of `App::submit_feedback()`.
pub fn queue_feedback(task: &Task, feedback: &str) -> Result<usize> {
    task.append_feedback_to_log(feedback)?;
    task.queue_feedback(feedback)?;
    Ok(task.queued_feedback_count())
}

/// Write immediate feedback for a stopped task.
///
/// Extracts the "stopped" branch of `App::submit_feedback()`.
/// Does NOT run `agman continue` — that's a side effect handled by the caller.
pub fn write_immediate_feedback(task: &Task, feedback: &str) -> Result<()> {
    task.write_feedback(feedback)
}

/// Delete a single queued feedback item by index.
pub fn delete_queued_feedback(task: &Task, index: usize) -> Result<()> {
    task.remove_feedback_queue_item(index)
}

/// Clear all queued feedback.
pub fn clear_all_queued_feedback(task: &Task) -> Result<()> {
    task.clear_feedback_queue()
}

/// List all tasks, sorted by status (running > input_needed > stopped) then by updated_at desc.
pub fn list_tasks(config: &Config) -> Result<Vec<Task>> {
    Task::list_all(config)
}

/// Save notes for a task.
pub fn save_notes(task: &Task, notes: &str) -> Result<()> {
    task.write_notes(notes)
}

/// Save TASK.md content for a task.
pub fn save_task_file(task: &Task, content: &str) -> Result<()> {
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
    match task.pop_feedback_queue()? {
        Some(feedback) => {
            task.write_feedback(&feedback)?;
            Ok(Some(feedback))
        }
        None => Ok(None),
    }
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
