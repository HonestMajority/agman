mod helpers;

use agman::repo_stats::RepoStats;
use agman::task::{Task, TaskStatus};
use agman::use_cases::{self, DeleteMode, WorktreeSource};
use helpers::{create_test_task, init_test_repo, test_config};

// ---------------------------------------------------------------------------
// Create task
// ---------------------------------------------------------------------------

#[test]
fn create_task_with_new_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "feat-branch",
        "Build the widget",
        "new",
        WorktreeSource::NewBranch,
        false,
    )
    .unwrap();

    // Task directory and meta exist
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.repo_name, "myrepo");
    assert_eq!(task.meta.branch_name, "feat-branch");
    assert_eq!(task.meta.status, TaskStatus::Running);
    assert_eq!(task.meta.flow_name, "new");

    // TASK.md written to worktree
    let task_content = task.read_task().unwrap();
    assert!(task_content.contains("Build the widget"));

    // Worktree exists
    assert!(task.meta.worktree_path.exists());

    // Repo stats incremented
    let stats = RepoStats::load(&config.repo_stats_path());
    assert_eq!(stats.counts.get("myrepo"), Some(&1));

    // Init files created
    assert!(task.dir.join("notes.md").exists());
    assert!(task.dir.join("agent.log").exists());

    // Default flows/prompts created
    assert!(config.flow_path("new").exists());
    assert!(config.prompt_path("planner").exists());
}

#[test]
fn create_task_with_existing_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create a fake existing worktree directory
    let wt_path = config.worktree_path("myrepo", "existing-branch");
    std::fs::create_dir_all(&wt_path).unwrap();

    // Also need a repo dir (for init_default_files to not fail on git excludes)
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "existing-branch",
        "Work on existing branch",
        "new",
        WorktreeSource::ExistingWorktree(wt_path.clone()),
        false,
    )
    .unwrap();

    assert_eq!(task.meta.worktree_path, wt_path);
    assert!(task.dir.join("meta.json").exists());
}

#[test]
fn create_task_with_review_after() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "feat-review",
        "Desc",
        "new",
        WorktreeSource::NewBranch,
        true,
    )
    .unwrap();

    assert!(task.meta.review_after);
}

// ---------------------------------------------------------------------------
// Delete task (everything)
// ---------------------------------------------------------------------------

#[test]
fn delete_task_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "to-delete",
        "desc",
        "new",
        WorktreeSource::NewBranch,
        false,
    )
    .unwrap();

    let task_dir = task.dir.clone();
    let worktree_path = task.meta.worktree_path.clone();
    assert!(task_dir.exists());
    assert!(worktree_path.exists());

    use_cases::delete_task(&config, task, DeleteMode::Everything).unwrap();

    // Task dir removed
    assert!(!task_dir.exists());
    // Worktree removed
    assert!(!worktree_path.exists());
}

// ---------------------------------------------------------------------------
// Delete task (task only)
// ---------------------------------------------------------------------------

#[test]
fn delete_task_only() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "task-only-del",
        "desc",
        "new",
        WorktreeSource::NewBranch,
        false,
    )
    .unwrap();

    let task_dir = task.dir.clone();
    let worktree_path = task.meta.worktree_path.clone();
    let task_md = worktree_path.join("TASK.md");
    assert!(task_md.exists());

    use_cases::delete_task(&config, task, DeleteMode::TaskOnly).unwrap();

    // Task dir removed
    assert!(!task_dir.exists());
    // TASK.md removed from worktree
    assert!(!task_md.exists());
    // Worktree directory itself still exists (branch preserved)
    assert!(worktree_path.exists());
}

// ---------------------------------------------------------------------------
// Stop task
// ---------------------------------------------------------------------------

#[test]
fn stop_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    assert_eq!(task.meta.status, TaskStatus::Running);

    use_cases::stop_task(&mut task).unwrap();

    assert_eq!(task.meta.status, TaskStatus::Stopped);
    assert!(task.meta.current_agent.is_none());

    // Persisted to disk
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.status, TaskStatus::Stopped);
}

#[test]
fn stop_task_already_stopped_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::Stopped).unwrap();

    // Should not error
    use_cases::stop_task(&mut task).unwrap();
    assert_eq!(task.meta.status, TaskStatus::Stopped);
}

// ---------------------------------------------------------------------------
// Resume after answering
// ---------------------------------------------------------------------------

#[test]
fn resume_after_answering() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::InputNeeded).unwrap();

    use_cases::resume_after_answering(&mut task).unwrap();

    assert_eq!(task.meta.status, TaskStatus::Running);

    // Persisted to disk
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.status, TaskStatus::Running);
}

#[test]
fn resume_after_answering_not_input_needed_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    // Status is Running, not InputNeeded
    use_cases::resume_after_answering(&mut task).unwrap();
    assert_eq!(task.meta.status, TaskStatus::Running);
}

// ---------------------------------------------------------------------------
// Submit feedback (queued — running task)
// ---------------------------------------------------------------------------

#[test]
fn queue_feedback_on_running_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");
    // task starts as Running

    let count = use_cases::queue_feedback(&task, "fix the button").unwrap();
    assert_eq!(count, 1);

    let count2 = use_cases::queue_feedback(&task, "also fix the header").unwrap();
    assert_eq!(count2, 2);

    let queue = task.read_feedback_queue();
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0], "fix the button");
    assert_eq!(queue[1], "also fix the header");

    // Feedback should also be logged to agent.log
    let log = task.read_agent_log().unwrap();
    assert!(log.contains("fix the button"));
    assert!(log.contains("also fix the header"));
}

// ---------------------------------------------------------------------------
// Submit feedback (immediate — stopped task)
// ---------------------------------------------------------------------------

#[test]
fn write_immediate_feedback_on_stopped_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::Stopped).unwrap();

    use_cases::write_immediate_feedback(&task, "please fix the bug").unwrap();

    let fb = task.read_feedback().unwrap();
    assert_eq!(fb, "please fix the bug");
}

// ---------------------------------------------------------------------------
// Delete queued feedback
// ---------------------------------------------------------------------------

#[test]
fn delete_queued_feedback() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("a").unwrap();
    task.queue_feedback("b").unwrap();
    task.queue_feedback("c").unwrap();

    use_cases::delete_queued_feedback(&task, 1).unwrap();

    let queue = task.read_feedback_queue();
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0], "a");
    assert_eq!(queue[1], "c");
}

// ---------------------------------------------------------------------------
// Clear all queued feedback
// ---------------------------------------------------------------------------

#[test]
fn clear_all_queued_feedback() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("a").unwrap();
    task.queue_feedback("b").unwrap();

    use_cases::clear_all_queued_feedback(&task).unwrap();

    assert!(task.read_feedback_queue().is_empty());
}

// ---------------------------------------------------------------------------
// Restart task
// ---------------------------------------------------------------------------

#[test]
fn restart_task_sets_flow_step_and_status() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::Stopped).unwrap();
    task.meta.flow_step = 0;
    task.save_meta().unwrap();

    use_cases::restart_task(&mut task, 2).unwrap();

    assert_eq!(task.meta.flow_step, 2);
    assert_eq!(task.meta.status, TaskStatus::Running);

    // Persisted to disk
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.flow_step, 2);
    assert_eq!(loaded.meta.status, TaskStatus::Running);
}

// ---------------------------------------------------------------------------
// Pop and apply feedback
// ---------------------------------------------------------------------------

#[test]
fn pop_and_apply_feedback_writes_first_queued_item() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("first feedback").unwrap();
    task.queue_feedback("second feedback").unwrap();

    let result = use_cases::pop_and_apply_feedback(&task).unwrap();
    assert_eq!(result, Some("first feedback".to_string()));

    // FEEDBACK.md written
    let fb = task.read_feedback().unwrap();
    assert_eq!(fb, "first feedback");

    // Queue now has one item
    let queue = task.read_feedback_queue();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0], "second feedback");
}

#[test]
fn pop_and_apply_feedback_empty_queue_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    let result = use_cases::pop_and_apply_feedback(&task).unwrap();
    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// List / refresh tasks
// ---------------------------------------------------------------------------

#[test]
fn list_tasks_sorted_by_status() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let _t1 = create_test_task(&config, "repo", "running");

    let mut t2 = create_test_task(&config, "repo", "stopped");
    t2.update_status(TaskStatus::Stopped).unwrap();

    let mut t3 = create_test_task(&config, "repo", "input");
    t3.update_status(TaskStatus::InputNeeded).unwrap();

    let tasks = use_cases::list_tasks(&config).unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0].meta.status, TaskStatus::Running);
    assert_eq!(tasks[1].meta.status, TaskStatus::InputNeeded);
    assert_eq!(tasks[2].meta.status, TaskStatus::Stopped);
}

// ---------------------------------------------------------------------------
// Save notes
// ---------------------------------------------------------------------------

#[test]
fn save_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    use_cases::save_notes(&task, "my important notes").unwrap();

    let notes = task.read_notes().unwrap();
    assert_eq!(notes, "my important notes");
}

// ---------------------------------------------------------------------------
// Save TASK.md
// ---------------------------------------------------------------------------

#[test]
fn save_task_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    use_cases::save_task_file(&task, "# Goal\nNew goal content\n").unwrap();

    let content = task.read_task().unwrap();
    assert_eq!(content, "# Goal\nNew goal content\n");
}

// ---------------------------------------------------------------------------
// List commands (stored commands)
// ---------------------------------------------------------------------------

#[test]
fn list_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.init_default_files(false).unwrap();

    let commands = use_cases::list_commands(&config).unwrap();
    assert!(!commands.is_empty());

    // Should contain the default commands
    let ids: Vec<&str> = commands.iter().map(|c| c.id.as_str()).collect();
    assert!(ids.contains(&"create-pr"));
    assert!(ids.contains(&"rebase"));
}

// ---------------------------------------------------------------------------
// Create review task
// ---------------------------------------------------------------------------

#[test]
fn create_review_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create a fake existing worktree
    let wt_path = config.worktree_path("myrepo", "review-branch");
    std::fs::create_dir_all(&wt_path).unwrap();

    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_review_task(
        &config,
        "myrepo",
        "review-branch",
        WorktreeSource::ExistingWorktree(wt_path),
    )
    .unwrap();

    // Task created with review description
    let task_content = task.read_task().unwrap();
    assert!(task_content.contains("Review branch review-branch"));
    assert_eq!(task.meta.repo_name, "myrepo");
    assert_eq!(task.meta.branch_name, "review-branch");

    // Repo stats incremented
    let stats = RepoStats::load(&config.repo_stats_path());
    assert_eq!(stats.counts.get("myrepo"), Some(&1));
}
