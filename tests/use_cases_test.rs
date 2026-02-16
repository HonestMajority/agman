mod helpers;

use agman::git::parse_github_owner_repo;
use agman::repo_stats::RepoStats;
use agman::task::{Task, TaskStatus};
use agman::use_cases::{self, DeleteMode, PrPollAction, WorktreeSource};
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
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    // Task directory and meta exist
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.name, "myrepo");
    assert_eq!(task.meta.branch_name, "feat-branch");
    assert_eq!(task.meta.status, TaskStatus::Running);
    assert_eq!(task.meta.flow_name, "new");

    // TASK.md written to task directory
    let task_content = task.read_task().unwrap();
    assert!(task_content.contains("Build the widget"));

    // Worktree exists
    assert!(task.meta.primary_repo().worktree_path.exists());

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

    assert_eq!(task.meta.primary_repo().worktree_path, wt_path);
    assert!(task.dir.join("meta.json").exists());
}

#[test]
fn create_task_reuses_existing_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    // Create a worktree via git (simulates a worktree that already exists on disk)
    let wt_path =
        agman::git::Git::create_worktree_quiet(&config, "myrepo", "reuse-branch", None).unwrap();
    assert!(wt_path.exists());

    // Now create a task with ExistingBranch — should reuse the worktree instead of failing
    let task = use_cases::create_task(
        &config,
        "myrepo",
        "reuse-branch",
        "Reuse existing worktree",
        "new",
        WorktreeSource::ExistingBranch,
        false,
    )
    .unwrap();

    assert_eq!(task.meta.primary_repo().worktree_path, wt_path);
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.branch_name, "reuse-branch");

    // TASK.md written to the reused worktree
    let task_content = task.read_task().unwrap();
    assert!(task_content.contains("Reuse existing worktree"));
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
        WorktreeSource::NewBranch { base_branch: None },
        true,
    )
    .unwrap();

    assert!(task.meta.review_after);
}

// ---------------------------------------------------------------------------
// Create task with custom base branch
// ---------------------------------------------------------------------------

#[test]
fn create_task_with_custom_base_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo_path = init_test_repo(&tmp, "myrepo");

    // Create a branch "feature-base" to use as the base
    std::process::Command::new("git")
        .args(["branch", "feature-base"])
        .current_dir(&repo_path)
        .output()
        .unwrap();

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "derived-branch",
        "Build on feature-base",
        "new",
        WorktreeSource::NewBranch {
            base_branch: Some("feature-base".to_string()),
        },
        false,
    )
    .unwrap();

    // Task directory and meta exist
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.branch_name, "derived-branch");

    // Worktree exists
    assert!(task.meta.primary_repo().worktree_path.exists());

    // TASK.md written
    let task_content = task.read_task().unwrap();
    assert!(task_content.contains("Build on feature-base"));
}

// ---------------------------------------------------------------------------
// Create setup-only task
// ---------------------------------------------------------------------------

#[test]
fn create_setup_only_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_setup_only_task(
        &config,
        "myrepo",
        "empty-branch",
        WorktreeSource::NewBranch { base_branch: None },
    )
    .unwrap();

    // Task directory and meta exist
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.name, "myrepo");
    assert_eq!(task.meta.branch_name, "empty-branch");

    // Status is Stopped (not Running)
    assert_eq!(task.meta.status, TaskStatus::Stopped);

    // Flow name is "none"
    assert_eq!(task.meta.flow_name, "none");

    // TASK.md exists in task directory with empty goal
    let task_content = task.read_task().unwrap();
    assert_eq!(task_content, "# Goal\n\n# Plan\n");

    // Worktree exists
    assert!(task.meta.primary_repo().worktree_path.exists());

    // Repo stats incremented
    let stats = RepoStats::load(&config.repo_stats_path());
    assert_eq!(stats.counts.get("myrepo"), Some(&1));

    // Init files created
    assert!(task.dir.join("notes.md").exists());
    assert!(task.dir.join("agent.log").exists());
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
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    let task_dir = task.dir.clone();
    let worktree_path = task.meta.primary_repo().worktree_path.clone();
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
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    let task_dir = task.dir.clone();
    let worktree_path = task.meta.primary_repo().worktree_path.clone();
    let task_md = task_dir.join("TASK.md");
    assert!(task_md.exists());

    use_cases::delete_task(&config, task, DeleteMode::TaskOnly).unwrap();

    // Task dir removed (including TASK.md which now lives in task dir)
    assert!(!task_dir.exists());
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
// Put on hold
// ---------------------------------------------------------------------------

#[test]
fn put_on_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::Stopped).unwrap();

    use_cases::put_on_hold(&mut task).unwrap();

    assert_eq!(task.meta.status, TaskStatus::OnHold);

    // Persisted to disk
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.status, TaskStatus::OnHold);
}

// ---------------------------------------------------------------------------
// Resume from hold
// ---------------------------------------------------------------------------

#[test]
fn resume_from_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::OnHold).unwrap();

    use_cases::resume_from_hold(&mut task).unwrap();

    assert_eq!(task.meta.status, TaskStatus::Stopped);

    // Persisted to disk
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.status, TaskStatus::Stopped);
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

    let mut t4 = create_test_task(&config, "repo", "held");
    t4.update_status(TaskStatus::OnHold).unwrap();

    let tasks = use_cases::list_tasks(&config);
    assert_eq!(tasks.len(), 4);
    assert_eq!(tasks[0].meta.status, TaskStatus::Running);
    assert_eq!(tasks[1].meta.status, TaskStatus::InputNeeded);
    assert_eq!(tasks[2].meta.status, TaskStatus::Stopped);
    assert_eq!(tasks[3].meta.status, TaskStatus::OnHold);
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
    assert_eq!(task.meta.name, "myrepo");
    assert_eq!(task.meta.branch_name, "review-branch");

    // Repo stats incremented
    let stats = RepoStats::load(&config.repo_stats_path());
    assert_eq!(stats.counts.get("myrepo"), Some(&1));
}

// ---------------------------------------------------------------------------
// Create task reuses existing worktree for existing branch
// ---------------------------------------------------------------------------

#[test]
fn create_task_reuses_existing_worktree_for_existing_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    // Create a task with a new branch (sets up worktree + branch)
    let task = use_cases::create_task(
        &config,
        "myrepo",
        "reuse-branch",
        "Original task",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    let worktree_path = task.meta.primary_repo().worktree_path.clone();
    assert!(worktree_path.exists());

    // Delete the task with TaskOnly mode (keeps worktree + branch)
    use_cases::delete_task(&config, task, DeleteMode::TaskOnly).unwrap();

    // Worktree still exists, but task metadata is gone
    assert!(worktree_path.exists());
    assert!(!config.task_dir("myrepo", "reuse-branch").exists());

    // Create a new task for the same branch with ExistingBranch source —
    // this should succeed by reusing the existing worktree
    let task2 = use_cases::create_task(
        &config,
        "myrepo",
        "reuse-branch",
        "Recreated task",
        "new",
        WorktreeSource::ExistingBranch,
        false,
    )
    .unwrap();

    assert_eq!(task2.meta.primary_repo().worktree_path, worktree_path);
    assert!(task2.dir.join("meta.json").exists());
    let content = task2.read_task().unwrap();
    assert!(content.contains("Recreated task"));
}

// ---------------------------------------------------------------------------
// PR poll action: merged PR
// ---------------------------------------------------------------------------

#[test]
fn pr_poll_action_merged() {
    let action = use_cases::determine_pr_poll_action(TaskStatus::Stopped, true, 0, Some(0));
    assert!(matches!(action, PrPollAction::DeleteMerged));
}

// ---------------------------------------------------------------------------
// PR poll action: new review
// ---------------------------------------------------------------------------

#[test]
fn pr_poll_action_new_review() {
    let action = use_cases::determine_pr_poll_action(TaskStatus::Stopped, false, 3, Some(2));
    assert!(matches!(action, PrPollAction::AddressReview { new_count: 3 }));
}

// ---------------------------------------------------------------------------
// PR poll action: first poll (None count)
// ---------------------------------------------------------------------------

#[test]
fn pr_poll_action_first_poll() {
    let action = use_cases::determine_pr_poll_action(TaskStatus::Stopped, false, 2, None);
    assert!(matches!(action, PrPollAction::None));
}

// ---------------------------------------------------------------------------
// PR poll action: same count (no change)
// ---------------------------------------------------------------------------

#[test]
fn pr_poll_action_no_change() {
    let action = use_cases::determine_pr_poll_action(TaskStatus::Stopped, false, 2, Some(2));
    assert!(matches!(action, PrPollAction::None));
}

// ---------------------------------------------------------------------------
// Set review_addressed flag
// ---------------------------------------------------------------------------

#[test]
fn set_review_addressed_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    assert!(!task.meta.review_addressed);

    use_cases::set_review_addressed(&mut task, true).unwrap();
    assert!(task.meta.review_addressed);

    // Reload and verify persistence
    task.reload_meta().unwrap();
    assert!(task.meta.review_addressed);
}

// ---------------------------------------------------------------------------
// Update last review count
// ---------------------------------------------------------------------------

#[test]
fn update_review_count() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    assert!(task.meta.last_review_count.is_none());

    use_cases::update_last_review_count(&mut task, 5).unwrap();
    assert_eq!(task.meta.last_review_count, Some(5));
}

// ---------------------------------------------------------------------------
// Set linked PR
// ---------------------------------------------------------------------------

#[test]
fn set_linked_pr() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    // Create a task with a real worktree (has a git repo)
    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "pr-branch",
        "Test PR linking",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    // Add an origin remote pointing to a GitHub URL
    let wt = task.meta.primary_repo().worktree_path.clone();
    std::process::Command::new("git")
        .args(["remote", "add", "origin", "https://github.com/testowner/testrepo.git"])
        .current_dir(&wt)
        .output()
        .unwrap();

    use_cases::set_linked_pr(&mut task, 42, &wt, true).unwrap();

    assert!(task.meta.linked_pr.is_some());
    let pr = task.meta.linked_pr.as_ref().unwrap();
    assert_eq!(pr.number, 42);
    assert_eq!(pr.url, "https://github.com/testowner/testrepo/pull/42");
    assert!(pr.owned);
}

// ---------------------------------------------------------------------------
// Clear linked PR
// ---------------------------------------------------------------------------

#[test]
fn clear_linked_pr_resets_review_state() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    // Set up a linked PR and review state
    task.set_linked_pr(10, "https://github.com/o/r/pull/10".to_string(), true)
        .unwrap();
    task.meta.review_addressed = true;
    task.meta.last_review_count = Some(3);
    task.save_meta().unwrap();

    use_cases::clear_linked_pr(&mut task).unwrap();

    assert!(task.meta.linked_pr.is_none());
    assert!(!task.meta.review_addressed);
    assert!(task.meta.last_review_count.is_none());

    // Verify persistence
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert!(loaded.meta.linked_pr.is_none());
    assert!(!loaded.meta.review_addressed);
    assert!(loaded.meta.last_review_count.is_none());
}

// ---------------------------------------------------------------------------
// Set linked PR owned flag
// ---------------------------------------------------------------------------

#[test]
fn set_linked_pr_owned_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "owned-flag-branch",
        "Test owned flag",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    let wt = task.meta.primary_repo().worktree_path.clone();
    std::process::Command::new("git")
        .args(["remote", "add", "origin", "https://github.com/testowner/testrepo.git"])
        .current_dir(&wt)
        .output()
        .unwrap();

    // Set as non-owned
    use_cases::set_linked_pr(&mut task, 42, &wt, false).unwrap();
    let pr = task.meta.linked_pr.as_ref().unwrap();
    assert_eq!(pr.number, 42);
    assert!(!pr.owned);

    // Set as owned
    use_cases::set_linked_pr(&mut task, 43, &wt, true).unwrap();
    let pr = task.meta.linked_pr.as_ref().unwrap();
    assert_eq!(pr.number, 43);
    assert!(pr.owned);

    // Verify persistence after reload
    task.reload_meta().unwrap();
    let pr = task.meta.linked_pr.as_ref().unwrap();
    assert_eq!(pr.number, 43);
    assert!(pr.owned);
}

// ---------------------------------------------------------------------------
// Multi-repo task creation
// ---------------------------------------------------------------------------

#[test]
fn create_multi_repo_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let parent_dir = tmp.path().join("repos");
    std::fs::create_dir_all(&parent_dir).unwrap();

    let task = use_cases::create_multi_repo_task(
        &config,
        "repos",
        "multi-feat",
        "Implement cross-repo feature",
        "new-multi",
        parent_dir.clone(),
        false,
    )
    .unwrap();

    // Task directory and meta exist
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.name, "repos");
    assert_eq!(task.meta.branch_name, "multi-feat");
    assert_eq!(task.meta.status, TaskStatus::Running);
    assert_eq!(task.meta.flow_name, "new-multi");

    // Repos starts empty (repo-inspector hasn't run yet)
    assert!(task.meta.repos.is_empty());

    // parent_dir is set
    assert_eq!(task.meta.parent_dir, Some(parent_dir));

    // TASK.md written to task dir
    let task_content = task.read_task().unwrap();
    assert!(task_content.contains("Implement cross-repo feature"));

    // Init files created
    assert!(task.dir.join("notes.md").exists());
    assert!(task.dir.join("agent.log").exists());

    // Default flows/prompts created (including new-multi)
    assert!(config.flow_path("new-multi").exists());
    assert!(config.prompt_path("repo-inspector").exists());
}

// ---------------------------------------------------------------------------
// Parse repos from TASK.md
// ---------------------------------------------------------------------------

#[test]
fn parse_repos_from_task_md() {
    let content = r#"# Goal
Build a cross-repo feature.

# Repos
- frontend: Contains the UI components
- backend: Contains the API
- shared-lib: Common types used by both

# Plan
(To be created by planner agent)
"#;

    let repos = use_cases::parse_repos_from_task_md(content);
    assert_eq!(repos, vec!["frontend", "backend", "shared-lib"]);
}

#[test]
fn parse_repos_from_task_md_empty() {
    let content = r#"# Goal
Just a goal.

# Plan
Some plan.
"#;

    let repos = use_cases::parse_repos_from_task_md(content);
    assert!(repos.is_empty());
}

#[test]
fn parse_repos_from_task_md_no_colon() {
    let content = r#"# Repos
- repo-without-rationale
- repo-with-rationale: some reason
"#;

    let repos = use_cases::parse_repos_from_task_md(content);
    assert_eq!(repos, vec!["repo-without-rationale", "repo-with-rationale"]);
}

// ---------------------------------------------------------------------------
// Multi-repo task deletion
// ---------------------------------------------------------------------------

#[test]
fn delete_multi_repo_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    // Create two repos
    let _repo1 = init_test_repo(&tmp, "repo-a");
    let _repo2 = init_test_repo(&tmp, "repo-b");

    let parent_dir = tmp.path().join("repos");

    // Create the multi-repo task
    let mut task = use_cases::create_multi_repo_task(
        &config,
        "repos",
        "multi-del",
        "Multi delete test",
        "new-multi",
        parent_dir,
        false,
    )
    .unwrap();

    // Manually populate repos (simulating what setup_repos_from_task_md would do)
    let wt_a = agman::git::Git::create_worktree_quiet(&config, "repo-a", "multi-del", None).unwrap();
    let wt_b = agman::git::Git::create_worktree_quiet(&config, "repo-b", "multi-del", None).unwrap();

    task.meta.repos = vec![
        agman::task::RepoEntry {
            repo_name: "repo-a".to_string(),
            worktree_path: wt_a.clone(),
            tmux_session: "(repo-a)__multi-del".to_string(),
        },
        agman::task::RepoEntry {
            repo_name: "repo-b".to_string(),
            worktree_path: wt_b.clone(),
            tmux_session: "(repo-b)__multi-del".to_string(),
        },
    ];
    task.save_meta().unwrap();

    let task_dir = task.dir.clone();
    assert!(task_dir.exists());
    assert!(wt_a.exists());
    assert!(wt_b.exists());

    use_cases::delete_task(&config, task, DeleteMode::Everything).unwrap();

    // Task dir removed
    assert!(!task_dir.exists());
    // Both worktrees removed
    assert!(!wt_a.exists());
    assert!(!wt_b.exists());
}

// ---------------------------------------------------------------------------
// Setup repos from TASK.md (post-hook)
// ---------------------------------------------------------------------------

#[test]
fn setup_repos_from_task_md() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    // Create two git repos under the parent dir
    let _repo1 = init_test_repo(&tmp, "frontend");
    let _repo2 = init_test_repo(&tmp, "backend");

    let parent_dir = tmp.path().join("repos");

    // Create a multi-repo task (starts with empty repos)
    let mut task = use_cases::create_multi_repo_task(
        &config,
        "repos",
        "cross-repo",
        "Build cross-repo feature",
        "new-multi",
        parent_dir,
        false,
    )
    .unwrap();

    assert!(task.meta.repos.is_empty());

    // Write TASK.md with a # Repos section (simulating repo-inspector output)
    task.write_task(
        "# Goal\nBuild cross-repo feature\n\n# Repos\n- frontend: UI components\n- backend: API server\n\n# Plan\n(To be created)\n",
    )
    .unwrap();

    // Run the setup_repos post-hook logic
    // Note: tmux calls will fail silently (no tmux in test), but worktree
    // creation and meta persistence should succeed.
    use_cases::setup_repos_from_task_md(&config, &mut task).unwrap();

    // Repos should be populated
    assert_eq!(task.meta.repos.len(), 2);
    assert_eq!(task.meta.repos[0].repo_name, "frontend");
    assert_eq!(task.meta.repos[1].repo_name, "backend");

    // Worktrees should exist
    assert!(task.meta.repos[0].worktree_path.exists());
    assert!(task.meta.repos[1].worktree_path.exists());

    // Meta should be persisted — reload from disk and verify
    let reloaded = Task::load(&config, "repos", "cross-repo").unwrap();
    assert_eq!(reloaded.meta.repos.len(), 2);
    assert_eq!(reloaded.meta.repos[0].repo_name, "frontend");
    assert_eq!(reloaded.meta.repos[1].repo_name, "backend");
}

// ---------------------------------------------------------------------------
// Config file loading
// ---------------------------------------------------------------------------

#[test]
fn config_file_sets_repos_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let base_dir = tmp.path().join(".agman");
    std::fs::create_dir_all(&base_dir).unwrap();

    let custom_repos = tmp.path().join("custom-repos");
    std::fs::create_dir_all(&custom_repos).unwrap();

    // Write a config.toml with custom repos_dir
    let config_toml = format!("repos_dir = {:?}\n", custom_repos.to_str().unwrap());
    std::fs::write(base_dir.join("config.toml"), config_toml).unwrap();

    let cf = agman::config::load_config_file(&base_dir);
    assert_eq!(
        cf.repos_dir.unwrap(),
        custom_repos.to_str().unwrap()
    );

    // Config::new with the loaded value should point to the custom dir
    let config = agman::config::Config::new(base_dir.clone(), custom_repos.clone());
    assert_eq!(config.repos_dir, custom_repos);
}

#[test]
fn config_file_missing_falls_back_to_default() {
    let tmp = tempfile::tempdir().unwrap();
    let base_dir = tmp.path().join(".agman");
    std::fs::create_dir_all(&base_dir).unwrap();

    // No config.toml — should return defaults
    let cf = agman::config::load_config_file(&base_dir);
    assert!(cf.repos_dir.is_none());
}

#[test]
fn save_and_load_config_file_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let base_dir = tmp.path().join(".agman");
    std::fs::create_dir_all(&base_dir).unwrap();

    let cf = agman::config::ConfigFile {
        repos_dir: Some("/tmp/my-repos".to_string()),
    };
    agman::config::save_config_file(&base_dir, &cf).unwrap();

    let loaded = agman::config::load_config_file(&base_dir);
    assert_eq!(loaded.repos_dir.unwrap(), "/tmp/my-repos");
}

// ---------------------------------------------------------------------------
// Check dependencies
// ---------------------------------------------------------------------------

#[test]
fn check_dependencies_finds_git() {
    let missing = use_cases::check_dependencies();
    // `git` is always available in test environments
    assert!(!missing.contains(&"git".to_string()));
}

// ---------------------------------------------------------------------------
// Parse GitHub owner/repo from remote URL
// ---------------------------------------------------------------------------

#[test]
fn parse_github_owner_repo_formats() {
    // HTTPS with .git
    let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets.git").unwrap();
    assert_eq!(owner, "acme");
    assert_eq!(repo, "widgets");

    // HTTPS without .git
    let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets").unwrap();
    assert_eq!(owner, "acme");
    assert_eq!(repo, "widgets");

    // SSH with .git
    let (owner, repo) = parse_github_owner_repo("git@github.com:acme/widgets.git").unwrap();
    assert_eq!(owner, "acme");
    assert_eq!(repo, "widgets");

    // SSH without .git
    let (owner, repo) = parse_github_owner_repo("git@github.com:acme/widgets").unwrap();
    assert_eq!(owner, "acme");
    assert_eq!(repo, "widgets");

    // Non-GitHub URL returns None
    assert!(parse_github_owner_repo("https://gitlab.com/acme/widgets.git").is_none());
}

// ---------------------------------------------------------------------------
// Task with slash in branch name
// ---------------------------------------------------------------------------

#[test]
fn create_task_with_slash_in_branch_name() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "chore/my-feature",
        "Fix something",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
    )
    .unwrap();

    // Task directory is flat (no nested dirs from the slash)
    let expected_dir = config.tasks_dir.join("myrepo--chore-my-feature");
    assert_eq!(task.dir, expected_dir);
    assert!(task.dir.join("meta.json").exists());

    // meta.json preserves the original branch name
    assert_eq!(task.meta.branch_name, "chore/my-feature");

    // list_tasks() finds the task
    let tasks = use_cases::list_tasks(&config);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].meta.branch_name, "chore/my-feature");

    // Task::load() works via the sanitized task_id
    let loaded = Task::load(&config, "myrepo", "chore/my-feature").unwrap();
    assert_eq!(loaded.meta.branch_name, "chore/my-feature");
}
