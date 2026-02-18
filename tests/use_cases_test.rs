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

    use_cases::set_linked_pr(&mut task, 42, &wt, true, None).unwrap();

    assert!(task.meta.linked_pr.is_some());
    let pr = task.meta.linked_pr.as_ref().unwrap();
    assert_eq!(pr.number, 42);
    assert_eq!(pr.url, "https://github.com/testowner/testrepo/pull/42");
    assert!(pr.owned);
    assert!(pr.author.is_none());
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
    task.set_linked_pr(10, "https://github.com/o/r/pull/10".to_string(), true, None)
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

    // Set as non-owned with author
    use_cases::set_linked_pr(&mut task, 42, &wt, false, Some("octocat".to_string())).unwrap();
    let pr = task.meta.linked_pr.as_ref().unwrap();
    assert_eq!(pr.number, 42);
    assert!(!pr.owned);
    assert_eq!(pr.author.as_deref(), Some("octocat"));

    // Set as owned (no author)
    use_cases::set_linked_pr(&mut task, 43, &wt, true, None).unwrap();
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
// Migrate old tasks — rewrites meta.json
// ---------------------------------------------------------------------------

#[test]
fn migrate_old_tasks_rewrites_meta_json() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create a task directory with old-format meta.json
    let task_dir = config.tasks_dir.join("myrepo--old-branch");
    std::fs::create_dir_all(&task_dir).unwrap();

    // Also create a fake worktree with TASK.md to test TASK.md migration
    let worktree_path = tmp.path().join("repos/myrepo-wt/old-branch");
    std::fs::create_dir_all(&worktree_path).unwrap();
    std::fs::write(worktree_path.join("TASK.md"), "# Goal\nOld task goal\n").unwrap();

    let old_meta = serde_json::json!({
        "repo_name": "myrepo",
        "branch_name": "old-branch",
        "status": "stopped",
        "tmux_session": "(myrepo)__old-branch",
        "worktree_path": worktree_path.to_str().unwrap(),
        "flow_name": "new",
        "current_agent": null,
        "flow_step": 0,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "review_after": false,
        "linked_pr": null,
        "last_review_count": null,
        "review_addressed": false
    });
    std::fs::write(
        task_dir.join("meta.json"),
        serde_json::to_string_pretty(&old_meta).unwrap(),
    )
    .unwrap();

    // Create init files that Task::load expects
    std::fs::write(task_dir.join("notes.md"), "").unwrap();
    std::fs::write(task_dir.join("agent.log"), "").unwrap();

    // Run migration
    use_cases::migrate_old_tasks(&config);

    // Verify meta.json was rewritten
    let migrated_content = std::fs::read_to_string(task_dir.join("meta.json")).unwrap();
    let migrated: serde_json::Value = serde_json::from_str(&migrated_content).unwrap();
    let obj = migrated.as_object().unwrap();

    // Has "name", not "repo_name" at top level
    assert_eq!(obj.get("name").unwrap().as_str().unwrap(), "myrepo");
    assert!(!obj.contains_key("repo_name"));

    // Has "repos" array with one entry
    let repos = obj.get("repos").unwrap().as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["repo_name"].as_str().unwrap(), "myrepo");
    assert_eq!(
        repos[0]["worktree_path"].as_str().unwrap(),
        worktree_path.to_str().unwrap()
    );
    assert_eq!(repos[0]["tmux_session"].as_str().unwrap(), "(myrepo)__old-branch");

    // No top-level tmux_session or worktree_path
    assert!(!obj.contains_key("tmux_session"));
    assert!(!obj.contains_key("worktree_path"));

    // Has parent_dir: null
    assert!(obj.get("parent_dir").unwrap().is_null());

    // Task::load() succeeds on migrated data
    let task = Task::load(&config, "myrepo", "old-branch").unwrap();
    assert_eq!(task.meta.name, "myrepo");
    assert_eq!(task.meta.branch_name, "old-branch");
    assert_eq!(task.meta.status, TaskStatus::Stopped);
    assert_eq!(task.meta.repos.len(), 1);
    assert_eq!(task.meta.repos[0].repo_name, "myrepo");

    // TASK.md was copied from worktree to task dir
    assert!(task_dir.join("TASK.md").exists());
    let task_content = std::fs::read_to_string(task_dir.join("TASK.md")).unwrap();
    assert!(task_content.contains("Old task goal"));
}

// ---------------------------------------------------------------------------
// Migrate old tasks — skips new format
// ---------------------------------------------------------------------------

#[test]
fn migrate_old_tasks_skips_new_format() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    // Capture meta.json content before migration
    let before = std::fs::read_to_string(task.dir.join("meta.json")).unwrap();

    // Run migration — should be a no-op
    use_cases::migrate_old_tasks(&config);

    // meta.json unchanged
    let after = std::fs::read_to_string(task.dir.join("meta.json")).unwrap();
    assert_eq!(before, after);

    // Task::load() still works
    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.name, "repo");
    assert_eq!(loaded.meta.branch_name, "branch");
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

// ---------------------------------------------------------------------------
// Directory classification (for repo picker)
// ---------------------------------------------------------------------------

#[test]
fn classify_directory_git_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let _repo = init_test_repo(&tmp, "myrepo");
    let repo_path = tmp.path().join("repos").join("myrepo");

    assert_eq!(
        use_cases::classify_directory(&repo_path),
        use_cases::DirKind::GitRepo
    );
}

#[test]
fn classify_directory_multi_repo_parent() {
    let tmp = tempfile::tempdir().unwrap();
    // Create a parent dir containing git repos
    let parent = tmp.path().join("repos").join("org");
    std::fs::create_dir_all(&parent).unwrap();
    // Create two git repos inside parent
    let child1 = parent.join("repo-a");
    std::fs::create_dir_all(child1.join(".git")).unwrap();
    let child2 = parent.join("repo-b");
    std::fs::create_dir_all(child2.join(".git")).unwrap();

    assert_eq!(
        use_cases::classify_directory(&parent),
        use_cases::DirKind::MultiRepoParent
    );
}

#[test]
fn classify_directory_plain() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("repos").join("empty-dir");
    std::fs::create_dir_all(&plain).unwrap();

    assert_eq!(
        use_cases::classify_directory(&plain),
        use_cases::DirKind::Plain
    );
}

#[test]
fn classify_directory_git_repo_takes_priority_over_children() {
    let tmp = tempfile::tempdir().unwrap();
    // A directory that is itself a git repo AND contains git-repo children
    let dir = tmp.path().join("repos").join("mixed");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let child = dir.join("sub-repo");
    std::fs::create_dir_all(child.join(".git")).unwrap();

    // .git presence should make it classify as GitRepo, not MultiRepoParent
    assert_eq!(
        use_cases::classify_directory(&dir),
        use_cases::DirKind::GitRepo
    );
}

// ---------------------------------------------------------------------------
// GitHub Notifications
// ---------------------------------------------------------------------------

#[test]
fn api_url_to_browser_url_transforms() {
    // PR URL: /pulls/ → /pull/
    assert_eq!(
        use_cases::api_url_to_browser_url(
            "https://api.github.com/repos/owner/repo/pulls/42",
            "https://github.com/owner/repo",
        ),
        "https://github.com/owner/repo/pull/42"
    );

    // Issue URL: /issues/ stays the same
    assert_eq!(
        use_cases::api_url_to_browser_url(
            "https://api.github.com/repos/owner/repo/issues/7",
            "https://github.com/owner/repo",
        ),
        "https://github.com/owner/repo/issues/7"
    );

    // Commit URL: /commits/ → /commit/
    assert_eq!(
        use_cases::api_url_to_browser_url(
            "https://api.github.com/repos/owner/repo/commits/abc123",
            "https://github.com/owner/repo",
        ),
        "https://github.com/owner/repo/commit/abc123"
    );

    // Empty URL falls back
    assert_eq!(
        use_cases::api_url_to_browser_url("", "https://github.com/owner/repo"),
        "https://github.com/owner/repo"
    );
}

#[test]
fn parse_notifications_json_extracts_fields() {
    let json = r#"[
        {
            "id": "123",
            "repository": { "full_name": "owner/repo" },
            "subject": {
                "title": "Fix the bug",
                "url": "https://api.github.com/repos/owner/repo/pulls/42",
                "type": "PullRequest"
            },
            "reason": "review_requested",
            "updated_at": "2025-01-01T00:00:00Z",
            "unread": true
        },
        {
            "id": "456",
            "repository": { "full_name": "other/project" },
            "subject": {
                "title": "Add feature",
                "url": null,
                "type": "Issue"
            },
            "reason": "mention",
            "updated_at": "2025-01-02T00:00:00Z",
            "unread": false
        }
    ]"#;

    let notifs = use_cases::parse_notifications_json(json);
    assert_eq!(notifs.len(), 2);

    assert_eq!(notifs[0].id, "123");
    assert_eq!(notifs[0].repo_full_name, "owner/repo");
    assert_eq!(notifs[0].title, "Fix the bug");
    assert_eq!(notifs[0].reason, "review_requested");
    assert_eq!(notifs[0].subject_type, "PullRequest");
    assert_eq!(notifs[0].unread, true);
    assert_eq!(notifs[0].browser_url, "https://github.com/owner/repo/pull/42");

    assert_eq!(notifs[1].id, "456");
    assert_eq!(notifs[1].repo_full_name, "other/project");
    assert_eq!(notifs[1].title, "Add feature");
    assert_eq!(notifs[1].reason, "mention");
    assert_eq!(notifs[1].subject_type, "Issue");
    assert_eq!(notifs[1].unread, false);
    // null subject.url falls back to repo URL
    assert_eq!(notifs[1].browser_url, "https://github.com/other/project");
}

// ---------------------------------------------------------------------------
// Notes: list_notes
// ---------------------------------------------------------------------------

#[test]
fn list_notes_ordering_and_filtering() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    // Create .md files
    std::fs::write(notes_dir.join("banana.md"), "").unwrap();
    std::fs::write(notes_dir.join("apple.md"), "").unwrap();
    // Create a non-.md file (should be excluded)
    std::fs::write(notes_dir.join("readme.txt"), "").unwrap();
    // Create a subdirectory
    std::fs::create_dir(notes_dir.join("projects")).unwrap();

    let entries = use_cases::list_notes(&notes_dir).unwrap();

    // Dirs come first, then files sorted alphabetically
    assert_eq!(entries.len(), 3);
    assert!(entries[0].is_dir);
    assert_eq!(entries[0].name, "projects");
    assert!(!entries[1].is_dir);
    assert_eq!(entries[1].name, "apple");
    assert_eq!(entries[1].file_name, "apple.md");
    assert!(!entries[2].is_dir);
    assert_eq!(entries[2].name, "banana");
    assert_eq!(entries[2].file_name, "banana.md");
}

// ---------------------------------------------------------------------------
// Notes: create_note
// ---------------------------------------------------------------------------

#[test]
fn create_note_adds_md_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    let path = use_cases::create_note(&notes_dir, "my-note").unwrap();
    assert!(path.exists());
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), "my-note.md");
}

// ---------------------------------------------------------------------------
// Notes: create_note_dir
// ---------------------------------------------------------------------------

#[test]
fn create_note_dir_creates_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    let path = use_cases::create_note_dir(&notes_dir, "projects").unwrap();
    assert!(path.exists());
    assert!(path.is_dir());
}

// ---------------------------------------------------------------------------
// Notes: delete_note
// ---------------------------------------------------------------------------

#[test]
fn delete_note_file_and_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    // Delete a file
    let file_path = notes_dir.join("to-delete.md");
    std::fs::write(&file_path, "content").unwrap();
    use_cases::delete_note(&file_path).unwrap();
    assert!(!file_path.exists());

    // Delete a directory with contents
    let dir_path = notes_dir.join("subdir");
    std::fs::create_dir(&dir_path).unwrap();
    std::fs::write(dir_path.join("child.md"), "").unwrap();
    use_cases::delete_note(&dir_path).unwrap();
    assert!(!dir_path.exists());
}

// ---------------------------------------------------------------------------
// Notes: rename_note
// ---------------------------------------------------------------------------

#[test]
fn rename_note_appends_md() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    let old_path = notes_dir.join("old-name.md");
    std::fs::write(&old_path, "content").unwrap();

    let new_path = use_cases::rename_note(&old_path, "new-name").unwrap();
    assert!(!old_path.exists());
    assert!(new_path.exists());
    assert_eq!(new_path.file_name().unwrap().to_str().unwrap(), "new-name.md");
}

// ---------------------------------------------------------------------------
// Notes: read_note / save_note
// ---------------------------------------------------------------------------

#[test]
fn read_and_save_note() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    let path = notes_dir.join("test.md");
    use_cases::save_note(&path, "Hello, world!").unwrap();

    let content = use_cases::read_note(&path).unwrap();
    assert_eq!(content, "Hello, world!");
}

// ---------------------------------------------------------------------------
// Notes: move_note
// ---------------------------------------------------------------------------

#[test]
fn move_note_down() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    std::fs::write(notes_dir.join("alpha.md"), "").unwrap();
    std::fs::write(notes_dir.join("beta.md"), "").unwrap();
    std::fs::write(notes_dir.join("gamma.md"), "").unwrap();

    // Move first file (alpha) down
    let new_idx = use_cases::move_note(&notes_dir, "alpha.md", use_cases::MoveDirection::Down).unwrap();
    assert_eq!(new_idx, 1);

    // Verify .order was written
    let order_content = std::fs::read_to_string(notes_dir.join(".order")).unwrap();
    let order_lines: Vec<&str> = order_content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(order_lines, vec!["beta.md", "alpha.md", "gamma.md"]);

    // Verify list_notes respects the new order
    let entries = use_cases::list_notes(&notes_dir).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].file_name, "beta.md");
    assert_eq!(entries[1].file_name, "alpha.md");
    assert_eq!(entries[2].file_name, "gamma.md");
}

#[test]
fn move_note_up() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    std::fs::write(notes_dir.join("alpha.md"), "").unwrap();
    std::fs::write(notes_dir.join("beta.md"), "").unwrap();
    std::fs::write(notes_dir.join("gamma.md"), "").unwrap();

    // Move last file (gamma) up
    let new_idx = use_cases::move_note(&notes_dir, "gamma.md", use_cases::MoveDirection::Up).unwrap();
    assert_eq!(new_idx, 1);

    let entries = use_cases::list_notes(&notes_dir).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].file_name, "alpha.md");
    assert_eq!(entries[1].file_name, "gamma.md");
    assert_eq!(entries[2].file_name, "beta.md");
}

#[test]
fn list_notes_respects_order_file() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    std::fs::write(notes_dir.join("alpha.md"), "").unwrap();
    std::fs::write(notes_dir.join("beta.md"), "").unwrap();
    std::fs::write(notes_dir.join("gamma.md"), "").unwrap();
    std::fs::create_dir(notes_dir.join("projects")).unwrap();

    // Hand-written .order that only mentions some entries
    std::fs::write(notes_dir.join(".order"), "gamma.md\nalpha.md\n").unwrap();

    let entries = use_cases::list_notes(&notes_dir).unwrap();
    assert_eq!(entries.len(), 4);
    // Ordered entries come first
    assert_eq!(entries[0].file_name, "gamma.md");
    assert_eq!(entries[1].file_name, "alpha.md");
    // Remaining entries: dirs first, then files, alphabetically
    assert_eq!(entries[2].file_name, "projects");
    assert!(entries[2].is_dir);
    assert_eq!(entries[3].file_name, "beta.md");
}
