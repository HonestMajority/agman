mod helpers;

use agman::git::parse_github_owner_repo;
use agman::project::Project;
use agman::repo_stats::RepoStats;
use agman::task::{QueueItem, SessionEntry, Task, TaskStatus};
use agman::use_cases::{self, GithubItemKind, PrPollAction, WorktreeSource};
use helpers::{
    create_test_project, create_test_task, init_test_repo, init_test_repo_at, test_config,
};

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
        None,
        None,
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
    assert!(config.prompt_path("coder").exists());
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
        None,
        None,
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
        agman::git::Git::create_worktree_quiet(&config, "myrepo", "reuse-branch", None, None)
            .unwrap();
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
    assert_eq!(task_content, "# Goal\n");

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
// Archive task
// ---------------------------------------------------------------------------

#[test]
fn archive_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "to-archive",
        "desc",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    let task_dir = task.dir.clone();
    let worktree_path = task.meta.primary_repo().worktree_path.clone();
    assert!(task_dir.exists());
    assert!(worktree_path.exists());

    use_cases::archive_task(&config, &mut task, false).unwrap();

    // Task dir still exists (archived, not deleted)
    assert!(task_dir.exists());
    // Worktree removed
    assert!(!worktree_path.exists());
    // archived_at is set
    assert!(task.meta.archived_at.is_some());
    // saved is false
    assert!(!task.meta.saved);

    // Branch is preserved (not deleted during archive)
    let branch_check = std::process::Command::new("git")
        .args(["branch", "--list", "to-archive"])
        .current_dir(&_repo_path)
        .output()
        .unwrap();
    assert!(
        !branch_check.stdout.is_empty(),
        "branch should still exist after archiving"
    );

    // Persisted to disk
    let loaded = Task::load(&config, "myrepo", "to-archive").unwrap();
    assert!(loaded.meta.archived_at.is_some());
    assert!(!loaded.meta.saved);
}

// ---------------------------------------------------------------------------
// Archive task with saved flag
// ---------------------------------------------------------------------------

#[test]
fn archive_task_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "to-save",
        "desc",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    use_cases::archive_task(&config, &mut task, true).unwrap();

    assert!(task.meta.archived_at.is_some());
    assert!(task.meta.saved);
}

// ---------------------------------------------------------------------------
// Permanently delete archived task
// ---------------------------------------------------------------------------

#[test]
fn permanently_delete_archived_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "perm-del",
        "desc",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    let task_dir = task.dir.clone();
    use_cases::archive_task(&config, &mut task, false).unwrap();
    assert!(task_dir.exists());

    // Branch should still exist after archive
    let branch_check = std::process::Command::new("git")
        .args(["branch", "--list", "perm-del"])
        .current_dir(&_repo_path)
        .output()
        .unwrap();
    assert!(
        !branch_check.stdout.is_empty(),
        "branch should exist after archive"
    );

    use_cases::permanently_delete_archived_task(&config, task).unwrap();
    assert!(!task_dir.exists());

    // Branch should be deleted after permanent delete
    let branch_check = std::process::Command::new("git")
        .args(["branch", "--list", "perm-del"])
        .current_dir(&_repo_path)
        .output()
        .unwrap();
    assert!(
        branch_check.stdout.is_empty(),
        "branch should be deleted after permanent delete"
    );
}

// ---------------------------------------------------------------------------
// Fully delete task
// ---------------------------------------------------------------------------

#[test]
fn fully_delete_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let task = use_cases::create_task(
        &config,
        "myrepo",
        "full-del",
        "desc",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    let task_dir = task.dir.clone();

    // Branch should exist after creation
    let branch_check = std::process::Command::new("git")
        .args(["branch", "--list", "full-del"])
        .current_dir(&_repo_path)
        .output()
        .unwrap();
    assert!(
        !branch_check.stdout.is_empty(),
        "branch should exist after creation"
    );

    use_cases::fully_delete_task(&config, task).unwrap();
    assert!(!task_dir.exists(), "task directory should be removed");

    // Branch should be deleted after full delete
    let branch_check = std::process::Command::new("git")
        .args(["branch", "--list", "full-del"])
        .current_dir(&_repo_path)
        .output()
        .unwrap();
    assert!(
        branch_check.stdout.is_empty(),
        "branch should be deleted after full delete"
    );
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

    use_cases::stop_task(&config, &mut task).unwrap();

    assert_eq!(task.meta.status, TaskStatus::Stopped);
    assert!(task.meta.current_agent.is_none());

    // honor_stop clears the .stop sentinel after handling it — the
    // supervisor pathway is synchronous now, so no sentinel should leak.
    assert!(
        !task.stop_path().exists(),
        ".stop sentinel should be cleared by honor_stop"
    );

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
    use_cases::stop_task(&config, &mut task).unwrap();
    assert_eq!(task.meta.status, TaskStatus::Stopped);
}

#[test]
fn stop_task_finalizes_live_session() {
    // A live session entry (stopped_at = None) must be finalized when the
    // user stops the task. Before this fix, stop_task bypassed the
    // supervisor and left session_history with dangling live entries.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.push_session(SessionEntry {
        agent: "coder".to_string(),
        name: "sid-live".to_string(),
        started_at: chrono::Utc::now(),
        stopped_at: None,
        condition: None,
        harness: agman::harness::HarnessKind::default(),
    })
    .unwrap();

    use_cases::stop_task(&config, &mut task).unwrap();

    let last = task.meta.session_history.last().unwrap();
    assert!(
        last.stopped_at.is_some(),
        "live session must be finalized on stop"
    );
    assert_eq!(last.condition.as_deref(), Some("STOPPED"));
}

#[test]
fn stop_task_restores_pre_command_flow_state() {
    // If the user stops a task mid-stored-command, the task's flow_name and
    // flow_step must be restored to the pre-command snapshot taken by
    // `drain_queue` so the task isn't stranded in the command flow.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.meta.flow_name = "create-pr".to_string();
    task.meta.flow_step = 1;
    task.meta.flow_sub_step = 0;
    task.meta.pre_command_flow_name = Some("new".to_string());
    task.meta.pre_command_flow_step = Some(4);
    task.save_meta().unwrap();

    use_cases::stop_task(&config, &mut task).unwrap();

    assert_eq!(task.meta.status, TaskStatus::Stopped);
    assert_eq!(task.meta.flow_name, "new");
    assert_eq!(task.meta.flow_step, 4);
    assert_eq!(task.meta.flow_sub_step, 0);
    assert!(task.meta.pre_command_flow_name.is_none());
    assert!(task.meta.pre_command_flow_step.is_none());
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
    let mut task = create_test_task(&config, "repo", "branch");
    // task starts as Running

    let count = use_cases::queue_feedback(&mut task, &config, "fix the button").unwrap();
    assert_eq!(count, 1);

    let count2 = use_cases::queue_feedback(&mut task, &config, "also fix the header").unwrap();
    assert_eq!(count2, 2);

    let queue = task.read_queue();
    assert_eq!(queue.len(), 2);
    match &queue[0] {
        QueueItem::Feedback { text } => assert_eq!(text, "fix the button"),
        _ => panic!("Expected Feedback item"),
    }
    match &queue[1] {
        QueueItem::Feedback { text } => assert_eq!(text, "also fix the header"),
        _ => panic!("Expected Feedback item"),
    }

    // Feedback should also be logged to agent.log
    let log = task.read_agent_log().unwrap();
    assert!(log.contains("fix the button"));
    assert!(log.contains("also fix the header"));
}

// ---------------------------------------------------------------------------
// queue_feedback on a Stopped task drains + wakes via the supervisor
// (TUI submit_feedback's Stopped branch).
// ---------------------------------------------------------------------------

#[test]
fn queue_feedback_on_stopped_task_drains_and_wakes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");
    task.update_status(TaskStatus::Stopped).unwrap();
    // Simulate an earlier run leaving the task on some flow past step 0.
    task.meta.flow_name = "new".to_string();
    task.meta.flow_step = 3;
    task.save_meta().unwrap();

    // queue_feedback on a Stopped task should drain (writing FEEDBACK.md,
    // switching to the `continue` flow, resetting the step, flipping to
    // Running) and then attempt to launch the next step. The launch itself
    // errors in the test env because no tmux is available — wake_if_idle's
    // error is warn-and-swallowed so queue_feedback still returns Ok.
    let _ = use_cases::queue_feedback(&mut task, &config, "please address the review").unwrap();

    // Queue was drained, not left pending.
    assert_eq!(task.read_queue().len(), 0);
    // Flow was switched to `continue` and reset to step 0.
    assert_eq!(task.meta.flow_name, "continue");
    assert_eq!(task.meta.flow_step, 0);
    // Task was flipped to Running by drain_queue.
    assert_eq!(task.meta.status, TaskStatus::Running);
    // Feedback was persisted to FEEDBACK.md for the refiner agent to read.
    let fb = task.read_feedback().unwrap();
    assert_eq!(fb, "please address the review");
}

// ---------------------------------------------------------------------------
// Delete queued feedback
// ---------------------------------------------------------------------------

#[test]
fn delete_queue_item() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("a").unwrap();
    task.queue_feedback("b").unwrap();
    task.queue_feedback("c").unwrap();

    use_cases::delete_queue_item(&task, 1).unwrap();

    let queue = task.read_queue();
    assert_eq!(queue.len(), 2);
    match &queue[0] {
        QueueItem::Feedback { text } => assert_eq!(text, "a"),
        _ => panic!("Expected Feedback item"),
    }
    match &queue[1] {
        QueueItem::Feedback { text } => assert_eq!(text, "c"),
        _ => panic!("Expected Feedback item"),
    }
}

// ---------------------------------------------------------------------------
// Clear all queued feedback
// ---------------------------------------------------------------------------

#[test]
fn clear_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("a").unwrap();
    task.queue_feedback("b").unwrap();

    use_cases::clear_queue(&task).unwrap();

    assert!(task.read_queue().is_empty());
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
// Queue command
// ---------------------------------------------------------------------------

#[test]
fn queue_command_on_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    let count = use_cases::queue_command(&mut task, &config, "rebase", Some("main")).unwrap();
    assert_eq!(count, 1);

    let queue = task.read_queue();
    assert_eq!(queue.len(), 1);
    match &queue[0] {
        QueueItem::Command { command_id, branch } => {
            assert_eq!(command_id, "rebase");
            assert_eq!(branch.as_deref(), Some("main"));
        }
        _ => panic!("Expected Command item"),
    }
}

// ---------------------------------------------------------------------------
// Queue command without branch
// ---------------------------------------------------------------------------

#[test]
fn queue_command_without_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    let count = use_cases::queue_command(&mut task, &config, "create-pr", None).unwrap();
    assert_eq!(count, 1);

    let queue = task.read_queue();
    assert_eq!(queue.len(), 1);
    match &queue[0] {
        QueueItem::Command { command_id, branch } => {
            assert_eq!(command_id, "create-pr");
            assert!(branch.is_none());
        }
        _ => panic!("Expected Command item"),
    }
}

// ---------------------------------------------------------------------------
// queue_command on a Stopped task drains via the supervisor and switches
// the task to the command flow (TUI run_branch_command / run_selected_command
// Stopped branches; replaces the old `agman run-command` shell-out path).
// ---------------------------------------------------------------------------

#[test]
fn queue_command_on_stopped_task_drains_and_wakes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    std::fs::write(
        config.command_path("rebase"),
        "name: Rebase\nid: rebase\ndescription: test\nrequires_arg: branch\nsteps:\n  - agent: rebaser\n    until: AGENT_DONE\n",
    )
    .unwrap();

    task.update_status(TaskStatus::Stopped).unwrap();
    task.meta.flow_name = "new".to_string();
    task.meta.flow_step = 2;
    task.save_meta().unwrap();

    // queue_command on a Stopped task should drain (snapshot the prior flow,
    // switch flow_name to the command id, write .branch-target if required,
    // flip to Running) and then attempt to launch. The launch errors in the
    // test env (no tmux); wake_if_idle warn-and-swallows so this returns Ok.
    let _ = use_cases::queue_command(&mut task, &config, "rebase", Some("main")).unwrap();

    assert_eq!(task.read_queue().len(), 0);
    assert_eq!(task.meta.flow_name, "rebase");
    assert_eq!(task.meta.flow_step, 0);
    assert_eq!(task.meta.status, TaskStatus::Running);
    assert_eq!(task.meta.pre_command_flow_name.as_deref(), Some("new"));
    assert_eq!(task.meta.pre_command_flow_step, Some(2));
    let branch_target = std::fs::read_to_string(task.dir.join(".branch-target")).unwrap();
    assert_eq!(branch_target, "main");
}

// ---------------------------------------------------------------------------
// Mixed queue (feedback + commands)
// ---------------------------------------------------------------------------

#[test]
fn mixed_queue_preserves_order() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("fix the bug").unwrap();
    task.queue_command("rebase", Some("main")).unwrap();
    task.queue_feedback("also fix the header").unwrap();

    let queue = task.read_queue();
    assert_eq!(queue.len(), 3);
    assert!(matches!(&queue[0], QueueItem::Feedback { text } if text == "fix the bug"));
    assert!(matches!(&queue[1], QueueItem::Command { command_id, .. } if command_id == "rebase"));
    assert!(matches!(&queue[2], QueueItem::Feedback { text } if text == "also fix the header"));
}

// ---------------------------------------------------------------------------
// Migrate old feedback_queue.json to queue.json
// ---------------------------------------------------------------------------

#[test]
fn migrate_old_feedback_queue_to_queue_json() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    // Write old-format feedback_queue.json manually
    let old_path = task.dir.join("feedback_queue.json");
    let old_content = serde_json::to_string(&vec!["old feedback 1", "old feedback 2"]).unwrap();
    std::fs::write(&old_path, old_content).unwrap();

    // Reading the queue should trigger migration
    let queue = task.read_queue();
    assert_eq!(queue.len(), 2);
    match &queue[0] {
        QueueItem::Feedback { text } => assert_eq!(text, "old feedback 1"),
        _ => panic!("Expected Feedback item"),
    }
    match &queue[1] {
        QueueItem::Feedback { text } => assert_eq!(text, "old feedback 2"),
        _ => panic!("Expected Feedback item"),
    }

    // Old file should be deleted
    assert!(!old_path.exists());
    // New file should exist
    assert!(task.dir.join("queue.json").exists());
}

// ---------------------------------------------------------------------------
// Mark task seen
// ---------------------------------------------------------------------------

#[test]
fn mark_task_seen() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "myrepo", "feat-seen");

    // New tasks start unseen
    assert!(!task.meta.seen);

    // Mark as seen
    use_cases::mark_task_seen(&mut task).unwrap();
    assert!(task.meta.seen);

    // Persisted to disk
    let reloaded = Task::load(&config, "myrepo", "feat-seen").unwrap();
    assert!(reloaded.meta.seen);

    // Transitioning to Stopped resets seen
    task.update_status(TaskStatus::Stopped).unwrap();
    assert!(!task.meta.seen);
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
// Toggle project hold
// ---------------------------------------------------------------------------

#[test]
fn toggle_project_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    // Create a project
    Project::create(&config, "myproject", "A test project").unwrap();

    // Toggle hold on — should set held = true
    use_cases::toggle_project_hold(&config, "myproject").unwrap();
    let loaded = Project::load_by_name(&config, "myproject").unwrap();
    assert!(loaded.meta.held);

    // Toggle hold off — should set held = false
    use_cases::toggle_project_hold(&config, "myproject").unwrap();
    let loaded = Project::load_by_name(&config, "myproject").unwrap();
    assert!(!loaded.meta.held);
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
        None,
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
    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "reuse-branch",
        "Original task",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    let worktree_path = task.meta.primary_repo().worktree_path.clone();
    assert!(worktree_path.exists());

    // Archive the task (removes worktree + branch, keeps task dir)
    use_cases::archive_task(&config, &mut task, false).unwrap();

    // Worktree is gone (archived removes it), task dir still exists but is archived
    assert!(!worktree_path.exists());
    assert!(config.task_dir("myrepo", "reuse-branch").exists());

    // Permanently delete the archived task to clean up the task dir
    use_cases::permanently_delete_archived_task(&config, task).unwrap();
    assert!(!config.task_dir("myrepo", "reuse-branch").exists());

    // Create a new task for the same branch with NewBranch source —
    // this should succeed by creating a fresh worktree
    let task2 = use_cases::create_task(
        &config,
        "myrepo",
        "reuse-branch",
        "Recreated task",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    assert!(task2.meta.primary_repo().worktree_path.exists());
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
    assert!(matches!(
        action,
        PrPollAction::AddressReview { new_count: 3 }
    ));
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
        None,
        None,
    )
    .unwrap();

    // Add an origin remote pointing to a GitHub URL
    let wt = task.meta.primary_repo().worktree_path.clone();
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/testowner/testrepo.git",
        ])
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
        None,
        None,
    )
    .unwrap();

    let wt = task.meta.primary_repo().worktree_path.clone();
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/testowner/testrepo.git",
        ])
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
        None,
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
(To be filled in by coder)
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
// Multi-repo task archive
// ---------------------------------------------------------------------------

#[test]
fn archive_multi_repo_task() {
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
        "Multi archive test",
        "new-multi",
        parent_dir,
        false,
        None,
    )
    .unwrap();

    // Manually populate repos (simulating what setup_repos_from_task_md would do)
    let wt_a =
        agman::git::Git::create_worktree_quiet(&config, "repo-a", "multi-del", None, None).unwrap();
    let wt_b =
        agman::git::Git::create_worktree_quiet(&config, "repo-b", "multi-del", None, None).unwrap();

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

    use_cases::archive_task(&config, &mut task, false).unwrap();

    // Task dir still exists (archived)
    assert!(task_dir.exists());
    // Both worktrees removed
    assert!(!wt_a.exists());
    assert!(!wt_b.exists());
    // archived_at is set
    assert!(task.meta.archived_at.is_some());

    // Branches are preserved in both repos
    for repo_name in &["repo-a", "repo-b"] {
        let branch_check = std::process::Command::new("git")
            .args(["branch", "--list", "multi-del"])
            .current_dir(config.repo_path(repo_name))
            .output()
            .unwrap();
        assert!(
            !branch_check.stdout.is_empty(),
            "branch should still exist in {repo_name} after archiving"
        );
    }
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
        None,
    )
    .unwrap();

    assert!(task.meta.repos.is_empty());

    // Write TASK.md with a # Repos section (simulating repo-inspector output)
    task.write_task(
        "# Goal\nBuild cross-repo feature\n\n# Repos\n- frontend: UI components\n- backend: API server\n\n# Plan\n(To be created)\n",
    )
    .unwrap();

    // Run the setup_repos post-hook logic (skip_tmux=true to avoid leaking real sessions)
    use_cases::setup_repos_from_task_md(&config, &mut task, true).unwrap();

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
    assert_eq!(
        repos[0]["tmux_session"].as_str().unwrap(),
        "(myrepo)__old-branch"
    );

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
    assert_eq!(cf.repos_dir.unwrap(), custom_repos.to_str().unwrap());

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
        archive_retention_days: None,
        telegram_bot_token: None,
        telegram_chat_id: None,
        harness: None,
    };
    agman::config::save_config_file(&base_dir, &cf).unwrap();

    let loaded = agman::config::load_config_file(&base_dir);
    assert_eq!(loaded.repos_dir.unwrap(), "/tmp/my-repos");
}

// ---------------------------------------------------------------------------
// Telegram Config
// ---------------------------------------------------------------------------

#[test]
fn load_telegram_config_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let (token, chat_id) = agman::use_cases::load_telegram_config(&config);
    assert!(token.is_none());
    assert!(chat_id.is_none());
}

#[test]
fn save_and_load_telegram_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Save archive retention first to verify it's preserved
    agman::use_cases::save_archive_retention(&config, 42).unwrap();

    agman::use_cases::save_telegram_config(
        &config,
        Some("bot123:ABC".to_string()),
        Some("-100999".to_string()),
    )
    .unwrap();

    let (token, chat_id) = agman::use_cases::load_telegram_config(&config);
    assert_eq!(token.unwrap(), "bot123:ABC");
    assert_eq!(chat_id.unwrap(), "-100999");

    // Verify other config fields are preserved
    let days = agman::use_cases::load_archive_retention(&config);
    assert_eq!(days, 42);
}

// ---------------------------------------------------------------------------
// Check dependencies
// ---------------------------------------------------------------------------

#[test]
fn check_dependencies_finds_git() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let missing = use_cases::check_dependencies(&config);
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
        None,
        None,
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
    assert!(notifs[0].unread);
    assert_eq!(
        notifs[0].browser_url,
        "https://github.com/owner/repo/pull/42"
    );

    assert_eq!(notifs[1].id, "456");
    assert_eq!(notifs[1].repo_full_name, "other/project");
    assert_eq!(notifs[1].title, "Add feature");
    assert_eq!(notifs[1].reason, "mention");
    assert_eq!(notifs[1].subject_type, "Issue");
    assert!(!notifs[1].unread);
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
    assert_eq!(
        new_path.file_name().unwrap().to_str().unwrap(),
        "new-name.md"
    );
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
    let new_idx =
        use_cases::move_note(&notes_dir, "alpha.md", use_cases::MoveDirection::Down).unwrap();
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
    let new_idx =
        use_cases::move_note(&notes_dir, "gamma.md", use_cases::MoveDirection::Up).unwrap();
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

#[test]
fn move_note_entry_not_in_order_file() {
    let tmp = tempfile::tempdir().unwrap();
    let notes_dir = tmp.path().join("notes");
    std::fs::create_dir_all(&notes_dir).unwrap();

    std::fs::write(notes_dir.join("alpha.md"), "").unwrap();
    std::fs::write(notes_dir.join("beta.md"), "").unwrap();
    std::fs::write(notes_dir.join("gamma.md"), "").unwrap();

    // Write a partial .order that only mentions alpha and beta
    std::fs::write(notes_dir.join(".order"), "alpha.md\nbeta.md\n").unwrap();

    // Move gamma (not in .order) up — should succeed, not error
    let new_idx =
        use_cases::move_note(&notes_dir, "gamma.md", use_cases::MoveDirection::Up).unwrap();
    assert_eq!(new_idx, 1);

    // Verify .order now contains all three entries with gamma moved up
    let order_content = std::fs::read_to_string(notes_dir.join(".order")).unwrap();
    let order_lines: Vec<&str> = order_content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(order_lines, vec!["alpha.md", "gamma.md", "beta.md"]);
}

#[test]
fn paste_note_moves_file_between_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("src_dir");
    let dest_dir = tmp.path().join("dest_dir");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();

    std::fs::write(src_dir.join("todo.md"), "buy milk").unwrap();
    std::fs::write(src_dir.join("other.md"), "").unwrap();

    // Set up .order in source with both entries
    std::fs::write(src_dir.join(".order"), "todo.md\nother.md\n").unwrap();
    // Set up .order in destination
    std::fs::write(dest_dir.join(".order"), "existing.md\n").unwrap();
    std::fs::write(dest_dir.join("existing.md"), "").unwrap();

    // Paste todo.md from src to dest
    use_cases::paste_note(&src_dir, &dest_dir, "todo.md").unwrap();

    // File moved
    assert!(!src_dir.join("todo.md").exists());
    assert!(dest_dir.join("todo.md").exists());
    assert_eq!(
        std::fs::read_to_string(dest_dir.join("todo.md")).unwrap(),
        "buy milk"
    );

    // Source .order no longer contains todo.md
    let src_order = std::fs::read_to_string(src_dir.join(".order")).unwrap();
    let src_lines: Vec<&str> = src_order.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(src_lines, vec!["other.md"]);

    // Dest .order has todo.md appended
    let dest_order = std::fs::read_to_string(dest_dir.join(".order")).unwrap();
    let dest_lines: Vec<&str> = dest_order.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(dest_lines, vec!["existing.md", "todo.md"]);
}

#[test]
fn paste_note_rejects_duplicate_name() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("src");
    let dest_dir = tmp.path().join("dest");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();

    std::fs::write(src_dir.join("readme.md"), "src").unwrap();
    std::fs::write(dest_dir.join("readme.md"), "dest").unwrap();

    let result = use_cases::paste_note(&src_dir, &dest_dir, "readme.md");
    assert!(result.is_err());
    // Original files unchanged
    assert_eq!(
        std::fs::read_to_string(src_dir.join("readme.md")).unwrap(),
        "src"
    );
    assert_eq!(
        std::fs::read_to_string(dest_dir.join("readme.md")).unwrap(),
        "dest"
    );
}

// ---------------------------------------------------------------------------
// Show PRs (parse_search_items_json)
// ---------------------------------------------------------------------------

#[test]
fn parse_search_items_json_issues() {
    let json = r#"[
        {
            "number": 42,
            "title": "Fix login bug",
            "repository": {"nameWithOwner": "acme/webapp"},
            "state": "open",
            "url": "https://github.com/acme/webapp/issues/42",
            "updatedAt": "2025-12-01T10:30:00Z",
            "author": {"login": "alice"}
        },
        {
            "number": 7,
            "title": "Add dark mode",
            "repository": {"nameWithOwner": "acme/ui"},
            "state": "open",
            "url": "https://github.com/acme/ui/issues/7",
            "updatedAt": "2025-11-20T08:00:00Z",
            "author": {"login": "bob"}
        }
    ]"#;

    let items = use_cases::parse_search_items_json(json, GithubItemKind::Issue);
    assert_eq!(items.len(), 2);

    assert_eq!(items[0].number, 42);
    assert_eq!(items[0].title, "Fix login bug");
    assert_eq!(items[0].repo_full_name, "acme/webapp");
    assert_eq!(items[0].state, "open");
    assert_eq!(items[0].url, "https://github.com/acme/webapp/issues/42");
    assert_eq!(items[0].updated_at, "2025-12-01T10:30:00Z");
    assert_eq!(items[0].author, "alice");
    assert!(!items[0].is_draft);
    assert_eq!(items[0].kind, GithubItemKind::Issue);

    assert_eq!(items[1].number, 7);
    assert_eq!(items[1].author, "bob");
    assert_eq!(items[1].kind, GithubItemKind::Issue);
}

#[test]
fn parse_search_items_json_prs() {
    let json = r#"[
        {
            "number": 101,
            "title": "Refactor auth module",
            "repository": {"nameWithOwner": "acme/backend"},
            "state": "open",
            "url": "https://github.com/acme/backend/pull/101",
            "updatedAt": "2025-12-05T14:00:00Z",
            "author": {"login": "carol"},
            "isDraft": true
        },
        {
            "number": 55,
            "title": "Update README",
            "repository": {"nameWithOwner": "acme/docs"},
            "state": "open",
            "url": "https://github.com/acme/docs/pull/55",
            "updatedAt": "2025-12-04T09:15:00Z",
            "author": {"login": "dave"},
            "isDraft": false
        }
    ]"#;

    let items = use_cases::parse_search_items_json(json, GithubItemKind::PullRequest);
    assert_eq!(items.len(), 2);

    assert_eq!(items[0].number, 101);
    assert_eq!(items[0].title, "Refactor auth module");
    assert!(items[0].is_draft);
    assert_eq!(items[0].kind, GithubItemKind::PullRequest);

    assert_eq!(items[1].number, 55);
    assert!(!items[1].is_draft);
    assert_eq!(items[1].kind, GithubItemKind::PullRequest);
}

// ---------------------------------------------------------------------------
// Toggle archive saved
// ---------------------------------------------------------------------------

#[test]
fn toggle_archive_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let mut task = use_cases::create_task(
        &config,
        "myrepo",
        "toggle-saved",
        "desc",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        None,
        None,
    )
    .unwrap();

    use_cases::archive_task(&config, &mut task, false).unwrap();
    assert!(!task.meta.saved);

    use_cases::toggle_archive_saved(&config, &mut task).unwrap();
    assert!(task.meta.saved);

    // Persisted to disk
    let loaded = Task::load(&config, "myrepo", "toggle-saved").unwrap();
    assert!(loaded.meta.saved);

    // Toggle back
    use_cases::toggle_archive_saved(&config, &mut task).unwrap();
    assert!(!task.meta.saved);
}

// ---------------------------------------------------------------------------
// Purge old archives
// ---------------------------------------------------------------------------

#[test]
fn purge_old_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create three archived tasks with different timestamps
    // 1. Old unsaved (should be purged)
    let mut old_unsaved = create_test_task(&config, "repo", "old-unsaved");
    old_unsaved.meta.archived_at = Some(chrono::Utc::now() - chrono::Duration::days(60));
    old_unsaved.meta.saved = false;
    old_unsaved.save_meta().unwrap();

    // 2. Old saved (should survive)
    let mut old_saved = create_test_task(&config, "repo", "old-saved");
    old_saved.meta.archived_at = Some(chrono::Utc::now() - chrono::Duration::days(60));
    old_saved.meta.saved = true;
    old_saved.save_meta().unwrap();

    // 3. Recent unsaved (should survive)
    let mut recent = create_test_task(&config, "repo", "recent");
    recent.meta.archived_at = Some(chrono::Utc::now() - chrono::Duration::days(5));
    recent.meta.saved = false;
    recent.save_meta().unwrap();

    // 4. Active task (not archived, should not be touched)
    let _active = create_test_task(&config, "repo", "active");

    let purged = use_cases::purge_old_archives(&config).unwrap();
    assert_eq!(purged, 1);

    // Old unsaved is gone
    assert!(!config.task_dir("repo", "old-unsaved").exists());
    // Old saved survives
    assert!(config.task_dir("repo", "old-saved").exists());
    // Recent survives
    assert!(config.task_dir("repo", "recent").exists());
    // Active survives
    assert!(config.task_dir("repo", "active").exists());
}

// ---------------------------------------------------------------------------
// List archived tasks
// ---------------------------------------------------------------------------

#[test]
fn list_archived_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create an active task
    let _active = create_test_task(&config, "repo", "active");

    // Create an archived task
    let mut archived = create_test_task(&config, "repo", "archived");
    archived.meta.archived_at = Some(chrono::Utc::now());
    archived.save_meta().unwrap();
    // Write TASK.md for the archived task
    archived.write_task("# Goal\nArchived task goal\n").unwrap();

    let results = use_cases::list_archived_tasks(&config);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0.meta.task_id(), "repo--archived");
    assert!(results[0].1.contains("Archived task goal"));
}

// ---------------------------------------------------------------------------
// Task::list_all excludes archived tasks
// ---------------------------------------------------------------------------

#[test]
fn list_all_excludes_archived_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create an active task
    let _active = create_test_task(&config, "repo", "active-list");

    // Create an archived task
    let mut archived = create_test_task(&config, "repo", "archived-list");
    archived.meta.archived_at = Some(chrono::Utc::now());
    archived.save_meta().unwrap();

    let tasks = use_cases::list_tasks(&config);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].meta.task_id(), "repo--active-list");
}

// ---------------------------------------------------------------------------
// Archive retention config
// ---------------------------------------------------------------------------

#[test]
fn archive_retention_default_when_no_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let days = use_cases::load_archive_retention(&config);
    assert_eq!(days, 30);
}

#[test]
fn archive_retention_config_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    use_cases::save_archive_retention(&config, 90).unwrap();
    let days = use_cases::load_archive_retention(&config);
    assert_eq!(days, 90);
}

// ---------------------------------------------------------------------------
// Copy repo files to worktree
// ---------------------------------------------------------------------------

#[test]
fn copy_repo_files_to_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo_path = init_test_repo(&tmp, "myrepo");

    // Write .env in the main repo
    std::fs::write(repo_path.join(".env"), "SECRET=abc123\n").unwrap();

    // Create a worktree directory (simulating worktree creation)
    let worktree_path = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree_path).unwrap();

    // Call the use-case function
    use_cases::copy_repo_files_to_worktree(&config, "myrepo", &worktree_path, None).unwrap();

    // Assert .env was copied with correct content
    assert_eq!(
        std::fs::read_to_string(worktree_path.join(".env")).unwrap(),
        "SECRET=abc123\n"
    );

    // Write a different .env in the worktree to test no-overwrite
    std::fs::write(worktree_path.join(".env"), "EXISTING=keep\n").unwrap();

    // Call again — should NOT overwrite the existing file
    use_cases::copy_repo_files_to_worktree(&config, "myrepo", &worktree_path, None).unwrap();

    assert_eq!(
        std::fs::read_to_string(worktree_path.join(".env")).unwrap(),
        "EXISTING=keep\n"
    );
}

// ---------------------------------------------------------------------------
// setup_repos_from_task_md — multi-repo with parent_dir != repos_dir
// ---------------------------------------------------------------------------

#[test]
fn setup_repos_from_task_md_multi_repo_different_parent_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp); // repos_dir = tmp/repos/

    // Create a separate parent directory that is NOT the same as repos_dir
    let other_repos = tmp.path().join("other-repos");
    std::fs::create_dir_all(&other_repos).unwrap();

    // Initialize two child git repos inside the separate parent directory
    let _repo1 = init_test_repo_at(&other_repos, "alpha");
    let _repo2 = init_test_repo_at(&other_repos, "beta");

    // Create a multi-repo task with parent_dir pointing to the separate directory
    let mut task = use_cases::create_multi_repo_task(
        &config,
        "other-repos",
        "cross-fix",
        "Fix across repos",
        "new-multi",
        other_repos.clone(),
        false,
        None,
    )
    .unwrap();

    assert!(task.meta.repos.is_empty());
    assert_eq!(task.meta.parent_dir.as_deref(), Some(other_repos.as_path()));

    // Write TASK.md with a # Repos section (simulating repo-inspector output)
    task.write_task(
        "# Goal\nFix across repos\n\n# Repos\n- alpha: first repo\n- beta: second repo\n\n# Plan\n(TBD)\n",
    )
    .unwrap();

    // Run the setup_repos post-hook logic (skip_tmux=true to avoid leaking real sessions)
    use_cases::setup_repos_from_task_md(&config, &mut task, true).unwrap();

    // Repos should be populated
    assert_eq!(task.meta.repos.len(), 2);
    assert_eq!(task.meta.repos[0].repo_name, "alpha");
    assert_eq!(task.meta.repos[1].repo_name, "beta");

    // Worktrees should exist under the OTHER parent dir, NOT under repos_dir
    assert!(task.meta.repos[0].worktree_path.exists());
    assert!(task.meta.repos[1].worktree_path.exists());

    // Worktrees should be under other-repos/<repo>-wt/<branch>/, not repos/<repo>-wt/
    assert!(task.meta.repos[0].worktree_path.starts_with(&other_repos));
    assert!(task.meta.repos[1].worktree_path.starts_with(&other_repos));
    assert!(!task.meta.repos[0]
        .worktree_path
        .starts_with(tmp.path().join("repos")));
    assert!(!task.meta.repos[1]
        .worktree_path
        .starts_with(tmp.path().join("repos")));
}

// ---------------------------------------------------------------------------
// Create single-repo task with repo outside repos_dir
// ---------------------------------------------------------------------------

#[test]
fn create_task_with_repo_outside_repos_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp); // repos_dir = tmp/repos/

    // Create a repo in a directory that is NOT under repos_dir
    let external_dir = tmp.path().join("external-repos");
    std::fs::create_dir_all(&external_dir).unwrap();
    let _repo_path = init_test_repo_at(&external_dir, "myrepo");

    // Pass parent_dir = external_dir (since the repo is not under repos_dir)
    let task = use_cases::create_task(
        &config,
        "myrepo",
        "feat-external",
        "Build the external widget",
        "new",
        WorktreeSource::NewBranch { base_branch: None },
        false,
        Some(external_dir.clone()),
        None,
    )
    .unwrap();

    // Task directory and meta exist
    assert!(task.dir.join("meta.json").exists());
    assert_eq!(task.meta.name, "myrepo");
    assert_eq!(task.meta.branch_name, "feat-external");

    // parent_dir is stored in meta
    assert_eq!(
        task.meta.parent_dir.as_deref(),
        Some(external_dir.as_path())
    );

    // Task is NOT multi-repo (single repo with external parent_dir)
    assert!(!task.meta.is_multi_repo());

    // Worktree should exist under external_dir, NOT under repos_dir
    let wt_path = &task.meta.primary_repo().worktree_path;
    assert!(wt_path.exists());
    assert!(wt_path.starts_with(&external_dir));
    assert!(!wt_path.starts_with(tmp.path().join("repos")));
}

// ---------------------------------------------------------------------------
// Project management
// ---------------------------------------------------------------------------

#[test]
fn create_project() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let project = agman::project::Project::create(&config, "my-project", "A test project").unwrap();

    assert_eq!(project.meta.name, "my-project");
    assert_eq!(project.meta.description, "A test project");
    assert!(project.dir.join("meta.json").exists());
}

#[test]
fn list_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    agman::project::Project::create(&config, "alpha", "First").unwrap();
    agman::project::Project::create(&config, "beta", "Second").unwrap();

    let projects = agman::project::Project::list_all(&config).unwrap();
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0].meta.name, "alpha");
    assert_eq!(projects[1].meta.name, "beta");
}

#[test]
fn list_projects_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let projects = agman::project::Project::list_all(&config).unwrap();
    assert!(projects.is_empty());
}

#[test]
fn create_project_invalid_name() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let result = agman::project::Project::create(&config, "has spaces", "bad");
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Inbox messaging
// ---------------------------------------------------------------------------

#[test]
fn inbox_append_and_read() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox_path = tmp.path().join("inbox.jsonl");

    let msg1 = agman::inbox::append_message(&inbox_path, "chief-of-staff", "Hello PM").unwrap();
    assert_eq!(msg1.seq, 1);
    assert_eq!(msg1.from, "chief-of-staff");

    let msg2 = agman::inbox::append_message(&inbox_path, "user", "Do this").unwrap();
    assert_eq!(msg2.seq, 2);

    let messages = agman::inbox::read_messages(&inbox_path).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message, "Hello PM");
    assert_eq!(messages[1].message, "Do this");
}

#[test]
fn inbox_undelivered_and_mark_delivered() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox_path = tmp.path().join("inbox.jsonl");
    let seq_path = tmp.path().join("inbox.seq");

    agman::inbox::append_message(&inbox_path, "chief-of-staff", "msg1").unwrap();
    agman::inbox::append_message(&inbox_path, "chief-of-staff", "msg2").unwrap();
    agman::inbox::append_message(&inbox_path, "chief-of-staff", "msg3").unwrap();

    // All should be undelivered
    let undelivered = agman::inbox::read_undelivered(&inbox_path, &seq_path).unwrap();
    assert_eq!(undelivered.len(), 3);

    // Mark first two as delivered
    agman::inbox::mark_delivered(&seq_path, 2).unwrap();

    // Only msg3 should be undelivered
    let undelivered = agman::inbox::read_undelivered(&inbox_path, &seq_path).unwrap();
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].seq, 3);
    assert_eq!(undelivered[0].message, "msg3");
}

// ---------------------------------------------------------------------------
// Task project field
// ---------------------------------------------------------------------------

#[test]
fn task_project_field_defaults_to_none() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");
    let task = create_test_task(&config, "myrepo", "feat-branch");
    assert!(task.meta.project.is_none());
}

#[test]
fn task_project_field_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");
    let mut task = create_test_task(&config, "myrepo", "feat-branch");

    task.meta.project = Some("my-project".to_string());
    task.save_meta().unwrap();

    let loaded = Task::load(&config, "myrepo", "feat-branch").unwrap();
    assert_eq!(loaded.meta.project.as_deref(), Some("my-project"));
}

// ---------------------------------------------------------------------------
// Use-case level project tests
// ---------------------------------------------------------------------------

#[test]
fn use_case_create_project() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // No initial-message → description is a human label only, inbox stays empty.
    let project = use_cases::create_project(&config, "my-proj", "A test project", None).unwrap();
    assert_eq!(project.meta.name, "my-proj");
    assert_eq!(project.meta.description, "A test project");

    // Project directory and meta should exist
    assert!(config.project_dir("my-proj").join("meta.json").exists());

    // Description is no longer auto-sent to the inbox.
    let messages = agman::inbox::read_messages(&config.project_inbox("my-proj")).unwrap();
    assert_eq!(messages.len(), 0);
}

#[test]
fn create_project_with_initial_message_seeds_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let project = use_cases::create_project(
        &config,
        "briefed-proj",
        "Briefed project",
        Some("Kick off with the design doc"),
    )
    .unwrap();
    assert_eq!(project.meta.name, "briefed-proj");

    // Initial-message lands in the PM inbox tagged from `chief-of-staff`.
    let messages = agman::inbox::read_messages(&config.project_inbox("briefed-proj")).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "chief-of-staff");
    assert_eq!(messages[0].message, "Kick off with the design doc");
}

#[test]
fn create_project_without_initial_message_leaves_inbox_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Both an empty description and no initial message → inbox stays empty.
    let project = use_cases::create_project(&config, "empty-proj", "", None).unwrap();
    assert_eq!(project.meta.name, "empty-proj");

    let messages = agman::inbox::read_messages(&config.project_inbox("empty-proj")).unwrap();
    assert_eq!(messages.len(), 0);

    // Even with a description, a missing initial-message means no inbox seed.
    let project2 =
        use_cases::create_project(&config, "labeled-proj", "Just a label", None).unwrap();
    assert_eq!(project2.meta.description, "Just a label");
    let messages2 = agman::inbox::read_messages(&config.project_inbox("labeled-proj")).unwrap();
    assert_eq!(messages2.len(), 0);
}

#[test]
fn use_case_send_message_to_chief_of_staff() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    use_cases::send_message(&config, "chief-of-staff", "pm-frontend", "Task complete").unwrap();

    let messages = agman::inbox::read_messages(&config.chief_of_staff_inbox()).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "pm-frontend");
    assert_eq!(messages[0].message, "Task complete");
}

#[test]
fn use_case_send_message_to_project() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create the project directory first. Description is a label only — inbox stays empty.
    use_cases::create_project(&config, "frontend", "Frontend project", None).unwrap();

    use_cases::send_message(&config, "frontend", "chief-of-staff", "Please start work").unwrap();

    let messages = agman::inbox::read_messages(&config.project_inbox("frontend")).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "chief-of-staff");
    assert_eq!(messages[0].message, "Please start work");
}

#[test]
fn use_case_send_message_rejects_unknown_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let result = use_cases::send_message(&config, "bogus:xyz", "pm", "hello");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("unknown target"));
}

#[test]
fn use_case_send_message_rejects_nonexistent_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let result = use_cases::send_message(&config, "task:ghost--branch", "pm", "hello");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("unknown task"));
}

#[test]
fn use_case_send_message_to_task_appends_to_task_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    let task = create_test_task(&config, "repo", "feature");
    let task_id = task.meta.task_id();

    let target = format!("task:{task_id}");
    use_cases::send_message(&config, &target, "pm", "nudge from pm").unwrap();

    let messages = agman::inbox::read_messages(&config.task_inbox(&task_id)).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "pm");
    assert_eq!(messages[0].message, "nudge from pm");
}

#[test]
fn use_case_send_message_rejects_nonexistent_project() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let result = use_cases::send_message(&config, "nonexistent", "chief-of-staff", "hello");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("unknown project"));
    assert!(!config.project_inbox("nonexistent").exists());
}

#[test]
fn use_case_list_project_tasks_filters_correctly() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");

    // Create two tasks, assign one to a project
    let mut task1 = create_test_task(&config, "myrepo", "feat-a");
    task1.meta.project = Some("proj-x".to_string());
    task1.save_meta().unwrap();

    let _task2 = create_test_task(&config, "myrepo", "feat-b");

    let project_tasks = use_cases::list_project_tasks(&config, "proj-x").unwrap();
    assert_eq!(project_tasks.len(), 1);
    assert_eq!(project_tasks[0].meta.branch_name, "feat-a");

    let unassigned = use_cases::list_unassigned_tasks(&config).unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].meta.branch_name, "feat-b");
}

#[test]
fn use_case_get_task_log_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");
    let task = create_test_task(&config, "myrepo", "feat-log");

    // Write some log lines
    let log_path = task.dir.join("agent.log");
    std::fs::write(&log_path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

    let tail = use_cases::get_task_log_tail(&config, "myrepo--feat-log", 3).unwrap();
    assert_eq!(tail, "line3\nline4\nline5");
}

#[test]
fn use_case_get_task_status_text() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");
    let task = create_test_task(&config, "myrepo", "feat-status");

    // Write a TASK.md with a Goal section
    std::fs::write(
        task.dir.join("TASK.md"),
        "# Goal\nImplement the widget feature for dashboard\n\n# Plan\n- [ ] step 1\n",
    )
    .unwrap();

    // Create a flow YAML matching the test task's flow_name ("new")
    std::fs::create_dir_all(&config.flows_dir).unwrap();
    std::fs::write(
        config.flows_dir.join("new.yaml"),
        "name: new\nsteps:\n  - agent: coder\n    until: TASK_COMPLETE\n",
    )
    .unwrap();

    let text = use_cases::get_task_status_text(&config, "myrepo--feat-status").unwrap();
    assert!(text.contains("myrepo--feat-status"));
    assert!(text.contains("running"));
    // Rich flow step with total count and agent name
    assert!(text.contains("step 1/1: coder"));
    // Elapsed time for running task
    assert!(text.contains("Running for:"));
    // Goal from TASK.md
    assert!(text.contains("Implement the widget feature for dashboard"));
}

#[test]
fn use_case_migrate_tasks_to_project() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");

    // Create a project to migrate into
    let _project = helpers::create_test_project(&config, "backend");

    // Create two unassigned tasks
    let task1 = create_test_task(&config, "myrepo", "feat-a");
    let task2 = create_test_task(&config, "myrepo", "feat-b");
    assert!(task1.meta.project.is_none());
    assert!(task2.meta.project.is_none());

    // Migrate both tasks
    let task_ids = vec![task1.meta.task_id(), task2.meta.task_id()];
    let count = use_cases::migrate_tasks_to_project(&config, "backend", &task_ids).unwrap();
    assert_eq!(count, 2);

    // Verify tasks are now assigned to the project
    let project_tasks = use_cases::list_project_tasks(&config, "backend").unwrap();
    assert_eq!(project_tasks.len(), 2);

    let unassigned = use_cases::list_unassigned_tasks(&config).unwrap();
    assert_eq!(unassigned.len(), 0);
}

#[test]
fn delete_project() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo = init_test_repo(&tmp, "myrepo");

    // Create a project
    let project = helpers::create_test_project(&config, "backend");
    assert!(project.dir.exists());

    // Create a task assigned to that project
    let mut task = create_test_task(&config, "myrepo", "feat-a");
    task.meta.project = Some("backend".to_string());
    task.save_meta().unwrap();

    // Create a researcher assigned to that project
    let researcher = helpers::create_test_researcher(&config, "backend", "explore-auth");
    assert_eq!(
        researcher.meta.status,
        agman::researcher::ResearcherStatus::Running
    );

    // Delete the project
    use_cases::delete_project(&config, "backend").unwrap();

    // Project directory should be gone
    assert!(!config.project_dir("backend").exists());

    // Task should now be archived
    let reloaded = agman::task::Task::load_by_id(&config, &task.meta.task_id()).unwrap();
    assert!(reloaded.meta.archived_at.is_some());

    // Researcher should now be archived
    let reloaded_researcher =
        agman::researcher::Researcher::load(config.researcher_dir("backend", "explore-auth"))
            .unwrap();
    assert_eq!(
        reloaded_researcher.meta.status,
        agman::researcher::ResearcherStatus::Archived
    );
}

// ---------------------------------------------------------------------------
// Aggregated status
// ---------------------------------------------------------------------------

#[test]
fn aggregated_status() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    // Create a flow YAML so total_steps can be resolved
    config.ensure_dirs().unwrap();
    let flow_yaml = r#"
name: new
steps:
  - agent: refiner
    until: AGENT_DONE
  - agent: coder
    until: AGENT_DONE
  - agent: reviewer
    until: TASK_COMPLETE
"#;
    std::fs::write(config.flow_path("new"), flow_yaml).unwrap();

    // Create a project with 2 tasks
    let _project = create_test_project(&config, "backend");

    let mut task1 = create_test_task(&config, "myrepo", "feat-a");
    task1.meta.project = Some("backend".to_string());
    task1.meta.status = TaskStatus::Running;
    task1.save_meta().unwrap();

    let mut task2 = create_test_task(&config, "myrepo", "feat-b");
    task2.meta.project = Some("backend".to_string());
    task2.meta.status = TaskStatus::Stopped;
    task2.save_meta().unwrap();

    // Create an unassigned task
    let _task3 = create_test_task(&config, "other", "experiment");

    let result = use_cases::aggregated_status(&config).unwrap();

    // Should have 1 project group
    assert_eq!(result.projects.len(), 1);
    assert_eq!(result.projects[0].name, "backend");
    assert_eq!(result.projects[0].tasks.len(), 2);

    // Check task statuses in the project group
    let statuses: Vec<_> = result.projects[0].tasks.iter().map(|t| t.status).collect();
    assert!(statuses.contains(&TaskStatus::Running));
    assert!(statuses.contains(&TaskStatus::Stopped));

    // Check total_steps is resolved from the flow
    for t in &result.projects[0].tasks {
        assert_eq!(t.total_steps, Some(3));
    }

    // Should have 1 unassigned task
    assert_eq!(result.unassigned.len(), 1);
    assert_eq!(result.unassigned[0].task_id, "other--experiment");
}

// ---------------------------------------------------------------------------
// Project status with archived counts
// ---------------------------------------------------------------------------

#[test]
fn project_status_with_archived() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let _project = create_test_project(&config, "backend");

    // Running task assigned to the project
    let mut task1 = create_test_task(&config, "repo", "feat-a");
    task1.meta.project = Some("backend".to_string());
    task1.meta.status = TaskStatus::Running;
    task1.save_meta().unwrap();

    // Stopped, non-archived tasks are still active project tasks
    let mut task2 = create_test_task(&config, "repo", "feat-b");
    task2.meta.project = Some("backend".to_string());
    task2.meta.status = TaskStatus::Stopped;
    task2.save_meta().unwrap();

    // Archived task assigned to the project
    let mut archived1 = create_test_task(&config, "repo", "old-feat");
    archived1.meta.project = Some("backend".to_string());
    archived1.meta.archived_at = Some(chrono::Utc::now());
    archived1.save_meta().unwrap();

    let result = use_cases::project_status(&config, "backend").unwrap();
    assert_eq!(result.total_tasks, 3);
    assert_eq!(result.active_tasks, 2);
    assert_eq!(result.archived_tasks, 1);
}

// ---------------------------------------------------------------------------
// Aggregated status with archived counts
// ---------------------------------------------------------------------------

#[test]
fn aggregated_status_with_archived() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let flow_yaml = r#"
name: new
steps:
  - agent: coder
    until: AGENT_DONE
"#;
    std::fs::write(config.flow_path("new"), flow_yaml).unwrap();

    // Project with one active task and one archived task
    let _project = create_test_project(&config, "frontend");

    let mut active = create_test_task(&config, "repo", "feat-x");
    active.meta.project = Some("frontend".to_string());
    active.meta.status = TaskStatus::Running;
    active.save_meta().unwrap();

    let mut proj_archived = create_test_task(&config, "repo", "old-x");
    proj_archived.meta.project = Some("frontend".to_string());
    proj_archived.meta.archived_at = Some(chrono::Utc::now());
    proj_archived.save_meta().unwrap();

    // Unassigned archived task
    let mut unassigned_archived = create_test_task(&config, "repo", "stale");
    unassigned_archived.meta.archived_at = Some(chrono::Utc::now());
    unassigned_archived.save_meta().unwrap();

    let result = use_cases::aggregated_status(&config).unwrap();

    assert_eq!(result.projects.len(), 1);
    assert_eq!(result.projects[0].tasks.len(), 1); // only active
    assert_eq!(result.projects[0].archived_count, 1);
    assert_eq!(result.archived_unassigned, 1);
}

// ---------------------------------------------------------------------------
// Researcher management
// ---------------------------------------------------------------------------

#[test]
fn use_case_create_researcher() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    let _project = create_test_project(&config, "myproj");

    let researcher = use_cases::create_researcher(
        &config,
        "myproj",
        "code-explorer",
        "Investigate API patterns",
        None,
        None,
        None,
    )
    .unwrap();

    assert!(researcher.dir.join("meta.json").exists());
    assert_eq!(researcher.meta.name, "code-explorer");
    assert_eq!(researcher.meta.project, "myproj");
    assert_eq!(researcher.meta.description, "Investigate API patterns");
    assert_eq!(
        researcher.meta.status,
        agman::researcher::ResearcherStatus::Running
    );
    assert!(researcher.meta.repo.is_none());
    assert!(researcher.meta.branch.is_none());
    assert!(researcher.meta.task_id.is_none());

    // Verify that the research description was written to the inbox
    let inbox_path = config.researcher_inbox("myproj", "code-explorer");
    let messages = agman::inbox::read_messages(&inbox_path).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "user");
    assert_eq!(messages[0].message, "Investigate API patterns");
}

#[test]
fn use_case_create_researcher_empty_description_no_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    let _project = create_test_project(&config, "myproj");

    use_cases::create_researcher(&config, "myproj", "quiet-one", "", None, None, None).unwrap();

    // No inbox file should be created for empty description
    let inbox_path = config.researcher_inbox("myproj", "quiet-one");
    assert!(!inbox_path.exists());
}

#[test]
fn use_case_list_researchers() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    let _proj1 = create_test_project(&config, "proj1");
    let _proj2 = create_test_project(&config, "proj2");

    use_cases::create_researcher(&config, "proj1", "r1", "desc1", None, None, None).unwrap();
    use_cases::create_researcher(&config, "proj1", "r2", "desc2", None, None, None).unwrap();
    use_cases::create_researcher(&config, "proj2", "r3", "desc3", None, None, None).unwrap();

    // List all
    let all = use_cases::list_researchers(&config, None).unwrap();
    assert_eq!(all.len(), 3);

    // List by project
    let proj1_only = use_cases::list_researchers(&config, Some("proj1")).unwrap();
    assert_eq!(proj1_only.len(), 2);
    assert!(proj1_only.iter().all(|r| r.meta.project == "proj1"));

    let proj2_only = use_cases::list_researchers(&config, Some("proj2")).unwrap();
    assert_eq!(proj2_only.len(), 1);
    assert_eq!(proj2_only[0].meta.name, "r3");
}

#[test]
fn use_case_archive_researcher() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    let _project = create_test_project(&config, "myproj");

    use_cases::create_researcher(
        &config,
        "myproj",
        "temp-research",
        "quick look",
        None,
        None,
        None,
    )
    .unwrap();

    use_cases::archive_researcher(&config, "myproj", "temp-research").unwrap();

    // Reload and check status
    let dir = config.researcher_dir("myproj", "temp-research");
    let researcher = agman::researcher::Researcher::load(dir).unwrap();
    assert_eq!(
        researcher.meta.status,
        agman::researcher::ResearcherStatus::Archived
    );
}

#[test]
fn use_case_send_message_to_researcher() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    let _project = create_test_project(&config, "myproj");

    use_cases::create_researcher(
        &config,
        "myproj",
        "investigator",
        "check logs",
        None,
        None,
        None,
    )
    .unwrap();

    use_cases::send_message(
        &config,
        "researcher:myproj--investigator",
        "myproj",
        "Please check the error logs",
    )
    .unwrap();

    // Verify inbox has the message
    let inbox_path = config.researcher_inbox("myproj", "investigator");
    let contents = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(contents.contains("Please check the error logs"));
    assert!(contents.contains("myproj"));
}

// ---------------------------------------------------------------------------
// Chief of Staff-level researchers
// ---------------------------------------------------------------------------

#[test]
fn use_case_create_chief_of_staff_researcher() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    // No project directory created — CoS researchers don't need one

    let researcher = use_cases::create_researcher(
        &config,
        "chief-of-staff",
        "my-researcher",
        "research question",
        None,
        None,
        None,
    )
    .unwrap();

    assert!(researcher.dir.join("meta.json").exists());
    assert_eq!(researcher.meta.name, "my-researcher");
    assert_eq!(researcher.meta.project, "chief-of-staff");
    assert_eq!(researcher.meta.description, "research question");
    assert_eq!(
        researcher.meta.status,
        agman::researcher::ResearcherStatus::Running
    );

    // Verify that the research description was written to the inbox
    let inbox_path = config.researcher_inbox("chief-of-staff", "my-researcher");
    let messages = agman::inbox::read_messages(&inbox_path).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "user");
    assert_eq!(messages[0].message, "research question");
}

// ---------------------------------------------------------------------------
// Agent handoff
// ---------------------------------------------------------------------------

#[test]
fn request_handoff_appends_inbox_message() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    // Create Chief of Staff dir and inbox
    let cos_dir = config.chief_of_staff_dir();
    std::fs::create_dir_all(&cos_dir).unwrap();
    let inbox_path = config.chief_of_staff_inbox();

    // Request handoff
    use_cases::request_handoff(&inbox_path, "system", &cos_dir).unwrap();

    // Verify inbox contains the handoff request with state_dir path
    let contents = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(contents.contains("[HANDOFF REQUEST]"));
    assert!(contents.contains(&cos_dir.display().to_string()));
    assert!(contents.contains("system"));
}

#[test]
fn handoff_file_mechanics() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let cos_dir = config.chief_of_staff_dir();
    std::fs::create_dir_all(&cos_dir).unwrap();

    // Write a handoff.md as if the agent wrote it
    let handoff_path = cos_dir.join("handoff.md");
    std::fs::write(
        &handoff_path,
        "# Handoff Summary\n\nCurrently monitoring project alpha.\nPending: review task-42 results.\n",
    )
    .unwrap();

    // Verify we can read the handoff content
    let content = std::fs::read_to_string(&handoff_path).unwrap();
    assert!(content.contains("project alpha"));
    assert!(content.contains("task-42"));

    // Verify handoff cleanup
    std::fs::remove_file(&handoff_path).unwrap();
    assert!(!handoff_path.exists());
}

#[test]
fn dotted_branch_name_gets_sanitized_tmux_session() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "myrepo", "bump-0.6.18");

    assert_eq!(task.meta.repos[0].tmux_session, "(myrepo)__bump-0_6_18");
}

// ---------------------------------------------------------------------------
// Inbox polling target enumeration (Bug 1 + Bug 2 — cross-project delivery)
// ---------------------------------------------------------------------------

#[test]
fn collect_inbox_poll_targets_enumerates_disk() {
    use agman::config::Config;
    use helpers::create_test_researcher;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    // Two projects, each with a Running researcher.
    let _alpha = create_test_project(&config, "alpha");
    let _beta = create_test_project(&config, "beta");
    let _r1 = create_test_researcher(&config, "alpha", "r1");
    let _r2 = create_test_researcher(&config, "beta", "r2");

    // Pretend every tmux session exists so the enumeration logic is exercised.
    let targets = use_cases::collect_inbox_poll_targets(&config, |_| true);

    let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"chief-of-staff"),
        "expected Chief of Staff target, got {names:?}"
    );
    assert!(
        names.contains(&"alpha"),
        "expected alpha PM target, got {names:?}"
    );
    assert!(
        names.contains(&"beta"),
        "expected beta PM target, got {names:?}"
    );
    assert!(
        names.contains(&"researcher:alpha--r1"),
        "expected alpha researcher target, got {names:?}"
    );
    assert!(
        names.contains(&"researcher:beta--r2"),
        "expected beta researcher target, got {names:?}"
    );

    // Paths match what Config returns for each name.
    for t in &targets {
        match t.name.as_str() {
            "chief-of-staff" => {
                assert_eq!(t.inbox_path, config.chief_of_staff_inbox());
                assert_eq!(t.seq_path, config.chief_of_staff_seq());
                assert_eq!(t.session_name, Config::chief_of_staff_tmux_session());
            }
            "alpha" => {
                assert_eq!(t.inbox_path, config.project_inbox("alpha"));
                assert_eq!(t.seq_path, config.project_seq("alpha"));
                assert_eq!(t.session_name, Config::pm_tmux_session("alpha"));
            }
            "beta" => {
                assert_eq!(t.inbox_path, config.project_inbox("beta"));
                assert_eq!(t.seq_path, config.project_seq("beta"));
                assert_eq!(t.session_name, Config::pm_tmux_session("beta"));
            }
            "researcher:alpha--r1" => {
                assert_eq!(t.inbox_path, config.researcher_inbox("alpha", "r1"));
                assert_eq!(t.seq_path, config.researcher_seq("alpha", "r1"));
                assert_eq!(
                    t.session_name,
                    Config::researcher_tmux_session("alpha", "r1")
                );
            }
            "researcher:beta--r2" => {
                assert_eq!(t.inbox_path, config.researcher_inbox("beta", "r2"));
                assert_eq!(t.seq_path, config.researcher_seq("beta", "r2"));
                assert_eq!(
                    t.session_name,
                    Config::researcher_tmux_session("beta", "r2")
                );
            }
            other => panic!("unexpected target: {other}"),
        }
    }
}

#[test]
fn collect_inbox_poll_targets_scopes_task_to_agman_window() {
    // Non-task targets (Chief of Staff, PM, researcher) carry `window = None`
    // — their sessions have a single window so window scoping is irrelevant.
    // Task targets carry `window = Some("agman")` because the interactive
    // claude lives in the `agman` window of the task's tmux session.
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let task = create_test_task(&config, "myrepo", "feat-x");
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let task_id = task.meta.task_id();

    let targets = use_cases::collect_inbox_poll_targets(&config, |s| {
        s == agman::config::Config::chief_of_staff_tmux_session() || s == task_session
    });

    let cos = targets
        .iter()
        .find(|t| t.name == "chief-of-staff")
        .expect("Chief of Staff target should be present");
    assert_eq!(
        cos.window, None,
        "Chief of Staff target must not be window-scoped"
    );

    let task_target = targets
        .iter()
        .find(|t| t.name == format!("task:{task_id}"))
        .expect("task target should be present for Running task");
    assert_eq!(
        task_target.window.as_deref(),
        Some("agman"),
        "task target must be scoped to the `agman` window"
    );
}

// ---------------------------------------------------------------------------
// Stalled-target threshold (Bug 4)
// ---------------------------------------------------------------------------

#[test]
fn stalled_targets_from_counts_honors_threshold() {
    use std::collections::HashMap;

    let mut counts: HashMap<String, u32> = HashMap::new();
    counts.insert("alpha".to_string(), 2);
    counts.insert("beta".to_string(), 5);
    counts.insert("chief-of-staff".to_string(), 6);

    let stalled = use_cases::stalled_targets_from_counts(&counts, 5);

    assert!(stalled.contains(&"beta"), "expected beta, got {stalled:?}");
    assert!(
        stalled.contains(&"chief-of-staff"),
        "expected chief-of-staff, got {stalled:?}"
    );
    assert!(
        !stalled.contains(&"alpha"),
        "alpha is below threshold, got {stalled:?}"
    );
    assert_eq!(stalled.len(), 2, "got {stalled:?}");
}

// ---------------------------------------------------------------------------
// Telegram health classifier
// ---------------------------------------------------------------------------

#[test]
fn classify_telegram_health_states() {
    use agman::use_cases::{classify_telegram_health, TelegramHealth};

    // Not configured → Disabled regardless of heartbeat.
    assert_eq!(
        classify_telegram_health(None, 1000, false),
        TelegramHealth::Disabled
    );
    assert_eq!(
        classify_telegram_health(Some(1000), 1000, false),
        TelegramHealth::Disabled
    );

    // Configured but no heartbeat yet → NeverPolled.
    assert_eq!(
        classify_telegram_health(None, 1000, true),
        TelegramHealth::NeverPolled
    );

    // Heartbeat fresh (<30s) → Healthy.
    assert_eq!(
        classify_telegram_health(Some(990), 1000, true),
        TelegramHealth::Healthy
    );

    // 30..=120s → Stale.
    assert_eq!(
        classify_telegram_health(Some(940), 1000, true),
        TelegramHealth::Stale
    );
    assert_eq!(
        classify_telegram_health(Some(880), 1000, true),
        TelegramHealth::Stale
    );

    // >120s → Dead.
    assert_eq!(
        classify_telegram_health(Some(800), 1000, true),
        TelegramHealth::Dead
    );
}

// ---------------------------------------------------------------------------
// Multi-agent Telegram chat
// ---------------------------------------------------------------------------

#[test]
fn append_message_concurrent_seqs() {
    use agman::inbox;
    use std::sync::Arc;
    use std::thread;

    let tmp = tempfile::tempdir().unwrap();
    let inbox_path = Arc::new(tmp.path().join("inbox.jsonl"));

    let threads: Vec<_> = (0..50)
        .map(|i| {
            let path = Arc::clone(&inbox_path);
            thread::spawn(move || {
                inbox::append_message(&path, "test", &format!("msg {i}")).unwrap();
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    let messages = inbox::read_messages(&inbox_path).unwrap();
    assert_eq!(messages.len(), 50, "expected exactly 50 messages");
    let mut seqs: Vec<u64> = messages.iter().map(|m| m.seq).collect();
    seqs.sort();
    let expected: Vec<u64> = (1..=50).collect();
    assert_eq!(
        seqs, expected,
        "seqs should be unique and contiguous 1..=50"
    );
}

#[test]
fn relative_agent_list_from_chief_of_staff() {
    use agman::researcher::{Researcher, ResearcherStatus};

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    create_test_project(&config, "alpha");
    create_test_project(&config, "beta");
    helpers::create_test_researcher(&config, "chief-of-staff", "live");
    let mut archived = helpers::create_test_researcher(&config, "chief-of-staff", "old");
    archived.meta.status = ResearcherStatus::Archived;
    archived.save_meta().unwrap();

    // Sanity-check the helper left the archived researcher archived.
    let archived_reload = Researcher::load(config.researcher_dir("chief-of-staff", "old")).unwrap();
    assert_eq!(archived_reload.meta.status, ResearcherStatus::Archived);

    let agents = use_cases::relative_agent_list(&config, "chief-of-staff");
    let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&"alpha"));
    assert!(ids.contains(&"beta"));
    assert!(ids.contains(&"researcher:chief-of-staff--live"));
    assert!(!ids.iter().any(|id| id.contains("old")));
    assert_eq!(agents.len(), 3);
}

#[test]
fn relative_agent_list_from_pm() {
    use agman::researcher::ResearcherStatus;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    create_test_project(&config, "alpha");
    helpers::create_test_researcher(&config, "alpha", "live");
    let mut archived = helpers::create_test_researcher(&config, "alpha", "old");
    archived.meta.status = ResearcherStatus::Archived;
    archived.save_meta().unwrap();

    let agents = use_cases::relative_agent_list(&config, "alpha");
    let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&"researcher:alpha--live"));
    assert!(ids.contains(&"chief-of-staff"));
    assert!(!ids.iter().any(|id| id.contains("old")));
    assert_eq!(agents.len(), 2);
}

#[test]
fn relative_agent_list_from_researcher() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    create_test_project(&config, "alpha");
    helpers::create_test_researcher(&config, "alpha", "live");

    let agents = use_cases::relative_agent_list(&config, "researcher:alpha--live");
    let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "chief-of-staff"]);
}

#[test]
fn relative_agent_list_from_chief_of_staff_researcher_no_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    helpers::create_test_researcher(&config, "chief-of-staff", "live");

    let agents = use_cases::relative_agent_list(&config, "researcher:chief-of-staff--live");
    let cos_count = agents.iter().filter(|a| a.id == "chief-of-staff").count();
    assert_eq!(cos_count, 1, "chief-of-staff must appear exactly once");
}

#[test]
fn read_current_agent_missing_falls_back_to_chief_of_staff() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    assert_eq!(use_cases::read_current_agent(&config), "chief-of-staff");
}

#[test]
fn read_current_agent_stale_falls_back_to_chief_of_staff() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let path = config.telegram_current_agent_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "nonexistent-project").unwrap();

    assert_eq!(use_cases::read_current_agent(&config), "chief-of-staff");
}

#[test]
fn current_agent_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    create_test_project(&config, "proj");
    create_test_project(&config, "proj2");

    use_cases::write_current_agent(&config, "proj").unwrap();
    assert_eq!(use_cases::read_current_agent(&config), "proj");

    use_cases::write_current_agent(&config, "proj2").unwrap();
    assert_eq!(use_cases::read_current_agent(&config), "proj2");
}

#[test]
fn pm_prompt_omits_telegram_section_when_disabled() {
    let prompt = use_cases::build_pm_prompt(false, "foo");
    assert!(prompt.contains("foo"));
    assert!(!prompt.contains("## Telegram"));
}

#[test]
fn pm_prompt_includes_telegram_section_when_enabled() {
    let prompt = use_cases::build_pm_prompt(true, "foo");
    assert!(prompt.contains("foo"));
    assert!(prompt.contains("## Telegram"));
}

#[test]
fn researcher_prompt_includes_correct_from() {
    let proj_prompt = use_cases::build_researcher_prompt(true, "proj", "bar");
    assert!(
        proj_prompt.contains(r#"--from "researcher:proj--bar""#),
        "expected project researcher prompt to include --from \"researcher:proj--bar\", got:\n{proj_prompt}"
    );

    let cos_prompt = use_cases::build_researcher_prompt(true, "chief-of-staff", "baz");
    assert!(
        cos_prompt.contains(r#"--from "researcher:chief-of-staff--baz""#),
        "expected chief-of-staff researcher prompt to include --from \"researcher:chief-of-staff--baz\", got:\n{cos_prompt}"
    );
}

// ---------------------------------------------------------------------------
// Supervisor: inbox-based work delivery to task agents
// ---------------------------------------------------------------------------

#[test]
fn start_agent_step_queues_inbox_work_directive() {
    // The task agent launch flow follows the Chief of Staff/PM/researcher pattern: the
    // system prompt holds identity, while the dynamic per-launch payload
    // (TASK.md, feedback, git context) is delivered through the task inbox.
    // This test verifies that `start_agent_step` queues exactly one
    // "supervisor" message containing the TASK.md body onto the task inbox,
    // even when the underlying tmux send_keys fails (the queue happens
    // before send_keys so the message survives a failed launch and the TUI
    // poller can deliver it on the next ready window).
    use agman::inbox;
    use agman::supervisor;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let mut task = create_test_task(&config, "myrepo", "feat-x");
    task.write_task("# Goal\nDo the inbox-delivered work.\n")
        .unwrap();

    // Provide a minimal prompt so Agent::load + build_inbox_message succeed
    // before send_keys errors.
    std::fs::write(config.prompt_path("coder"), "You are a test coder.\n").unwrap();

    // send_keys fails (no real tmux session in tests) — but the inbox queue
    // happens before send_keys, so the message must still be present.
    let _ = supervisor::start_agent_step(&config, &mut task, "coder");

    let inbox_path = config.task_inbox(&task.meta.task_id());
    let messages = inbox::read_messages(&inbox_path).unwrap();
    assert_eq!(
        messages.len(),
        1,
        "exactly one work directive should be queued"
    );
    let msg = &messages[0];
    assert_eq!(msg.from, "supervisor");
    assert!(
        msg.message.contains("Do the inbox-delivered work"),
        "inbox message should contain TASK.md body, got:\n{}",
        msg.message
    );
    assert!(
        msg.message.contains("# Current Task"),
        "inbox message should contain the Current Task heading"
    );

    // The system prompt is no longer written to disk — it's passed inline
    // to the harness via build_session_command. Verify the inline shape
    // directly via Agent::build_system_prompt.
    use agman::agent::Agent;
    use agman::harness::HarnessKind;
    let agent = Agent::load(&config, "coder").unwrap();
    let system_prompt = agent
        .build_system_prompt(&task, false, HarnessKind::Claude.select().as_ref())
        .unwrap();
    assert!(
        !system_prompt.contains("Do the inbox-delivered work"),
        "system prompt must not embed TASK.md content"
    );
    assert!(
        system_prompt.contains("Work Directives"),
        "system prompt should include the work-directives preamble"
    );
}

#[test]
fn start_agent_step_with_pi_prepares_identity_file_and_session_dir() {
    use agman::harness::{self, HarnessKind};
    use agman::inbox;
    use agman::supervisor;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();
    use_cases::save_harness(&config, HarnessKind::Pi).unwrap();

    let mut task = create_test_task(&config, "myrepo", "feat-pi");
    task.write_task("# Goal\nDo the Pi launch work.\n").unwrap();
    std::fs::write(config.prompt_path("coder"), "You are a Pi coder.\n").unwrap();

    let result = supervisor::start_agent_step(&config, &mut task, "coder");
    assert!(result.is_err(), "tmux send_keys must fail in test env");

    let session_name = format!("agman-task-{}-step-1", task.meta.task_id());
    let identity_file = harness::pi::identity_file_path(&task.dir, &session_name);
    assert!(identity_file.exists(), "pi identity file must be prepared");
    assert!(
        std::fs::read_to_string(&identity_file)
            .unwrap()
            .contains("You are a Pi coder."),
        "identity file should contain the agent system prompt"
    );
    assert!(
        harness::pi::task_session_dir(&task.dir, &session_name).exists(),
        "pi per-step session dir must be prepared"
    );

    let messages = inbox::read_messages(&config.task_inbox(&task.meta.task_id())).unwrap();
    assert_eq!(messages.len(), 1, "work directive should still be queued");
    assert!(
        task.meta.session_history.is_empty(),
        "failed tmux launch must not push a SessionEntry"
    );
    let raw_meta = std::fs::read_to_string(task.dir.join("meta.json")).unwrap();
    assert!(
        !raw_meta.contains("\"harness\""),
        "TaskMeta must not grow a root harness field"
    );
}

// ---------------------------------------------------------------------------
// Project templates
// ---------------------------------------------------------------------------

#[test]
fn create_template_writes_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let body = "Release cleanup template\n\nUse this when shipping a feature flag.\n";
    agman::templates::write_template(&config, "release-cleanup", body).unwrap();

    let path = config.template_path("release-cleanup");
    assert!(path.exists());
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, body);
}

#[test]
fn list_templates_returns_name_and_description() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    agman::templates::write_template(
        &config,
        "alpha",
        "Alpha description line\n\nMore detail below.\n",
    )
    .unwrap();
    agman::templates::write_template(&config, "beta", "\n   \nBeta description line\n").unwrap();

    let summaries = agman::templates::list_templates(&config).unwrap();
    assert_eq!(summaries.len(), 2);
    // Sorted by name.
    assert_eq!(summaries[0].name, "alpha");
    assert_eq!(summaries[0].description, "Alpha description line");
    assert_eq!(summaries[1].name, "beta");
    // First non-empty trimmed line wins, even after blank/whitespace lines.
    assert_eq!(summaries[1].description, "Beta description line");
}

#[test]
fn get_template_returns_body() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let body = "# Template\n\nFull body, multiple lines.\n";
    agman::templates::write_template(&config, "my-template", body).unwrap();

    let read_back = agman::templates::read_template(&config, "my-template").unwrap();
    assert_eq!(read_back, body);
}

// ---------------------------------------------------------------------------
// Long-lived agent resume support (CEO/PM/researcher)
// ---------------------------------------------------------------------------

#[test]
fn long_lived_first_launch_claude_stamps_session_id() {
    // First launch under claude mints a UUID and writes it to
    // <state_dir>/session-id, returning Pin(uuid) so the launch command
    // pins claude's session via --session-id <uuid>.
    use agman::harness::{HarnessKind, LaunchContext, SessionKey};
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    let cwd = tmp.path();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        cwd,
        HarnessKind::Claude,
        false,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "pin");
    assert!(prep.is_first_launch);
    assert!(prep.session_name.starts_with("agman-chief-of-staff-"));
    let uuid = prep.handle.expect("claude first launch must have a uuid");

    // session-id stamped on disk and matches the returned handle.
    let id_path = state_dir.join("session-id");
    assert!(id_path.exists(), "claude session-id must be stamped");
    let stamped = std::fs::read_to_string(&id_path).unwrap();
    assert_eq!(stamped.trim(), uuid);
    assert_eq!(
        std::fs::read_to_string(state_dir.join("session-name"))
            .unwrap()
            .trim(),
        prep.session_name
    );

    // Build the launch command with the right session_key and assert
    // shape — proves the first-launch path threads through correctly.
    let h = HarnessKind::Claude.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: &prep.session_name,
        identity_file: None,
        session_dir: None,
        cwd: &prep.cwd,
        no_alt_screen: false,
        session_key: SessionKey::Pin(&uuid),
    });
    assert!(cmd.contains(&format!("--session-id '{uuid}'")));
    assert!(cmd.contains(&format!("--name '{}'", prep.session_name)));
}

#[test]
fn long_lived_first_launch_codex_stamps_session_name_and_launch_cwd() {
    // First launch under codex stamps <state_dir>/launch-cwd with the
    // resolved cwd, stamps a unique session-name, and returns Auto (no
    // launch-time pin; the unique name is registered post-launch via /rename).
    use agman::harness::HarnessKind;
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    let cwd = tmp.path();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        cwd,
        HarnessKind::Codex,
        false,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "auto");
    assert!(prep.is_first_launch);
    assert!(prep.handle.is_none());
    assert!(prep.session_name.starts_with("agman-chief-of-staff-"));
    assert_eq!(
        std::fs::read_to_string(state_dir.join("session-name"))
            .unwrap()
            .trim(),
        prep.session_name
    );

    let cwd_path = state_dir.join("launch-cwd");
    assert!(cwd_path.exists(), "codex launch-cwd must be stamped");
    let stamped = std::fs::read_to_string(&cwd_path).unwrap();
    assert_eq!(stamped.trim(), cwd.to_string_lossy());
}

#[test]
fn long_lived_resume_claude_emits_resume_flag() {
    // Pre-stamp <state_dir>/session-id and expect Resume(uuid).
    use agman::harness::{HarnessKind, LaunchContext, SessionKey};
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let pinned_uuid = "deadbeef-1234-5678-9abc-def012345678";
    let stamped_name = "agman-chief-of-staff-260101-000000-deadbeef";
    std::fs::write(state_dir.join("session-id"), pinned_uuid).unwrap();
    std::fs::write(state_dir.join("session-name"), stamped_name).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Claude,
        false,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "resume");
    assert!(!prep.is_first_launch);
    assert_eq!(prep.handle.as_deref(), Some(pinned_uuid));
    assert_eq!(prep.session_name, stamped_name);

    let h = HarnessKind::Claude.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: &prep.session_name,
        identity_file: None,
        session_dir: None,
        cwd: &prep.cwd,
        no_alt_screen: false,
        session_key: SessionKey::Resume(pinned_uuid),
    });
    assert!(cmd.contains(&format!("--resume '{pinned_uuid}'")));
    assert!(!cmd.contains("--system-prompt"));
}

#[test]
fn long_lived_resume_codex_emits_resume_subcommand() {
    // Pre-stamp session-name and assert that prepare returns Resume + the
    // launch command emits `codex resume <name>`.
    use agman::harness::{HarnessKind, LaunchContext, SessionKey};
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let stamped_name = "agman-chief-of-staff-260101-000000-c0dec0de";
    std::fs::write(state_dir.join("session-name"), stamped_name).unwrap();
    // Pre-stamp launch-cwd to a real directory so the resume picks it up.
    let stamped_cwd = tmp.path().join("worktree");
    std::fs::create_dir_all(&stamped_cwd).unwrap();
    std::fs::write(
        state_dir.join("launch-cwd"),
        stamped_cwd.to_string_lossy().as_ref(),
    )
    .unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        tmp.path(), // freshly-resolved cwd, should be ignored in favour of stamped
        HarnessKind::Codex,
        false,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "resume");
    assert!(!prep.is_first_launch);
    assert_eq!(prep.handle.as_deref(), Some(stamped_name));
    assert_eq!(prep.session_name, stamped_name);
    assert_eq!(prep.cwd, stamped_cwd);

    let h = HarnessKind::Codex.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: &prep.session_name,
        identity_file: None,
        session_dir: None,
        cwd: &prep.cwd,
        no_alt_screen: true,
        session_key: SessionKey::Resume(&prep.session_name),
    });
    assert!(cmd.contains(&format!(" resume '{stamped_name}'")));
    assert!(cmd.contains(&format!(" -C '{}'", stamped_cwd.to_string_lossy())));
    assert!(!cmd.contains("developer_instructions"));
}

#[test]
fn long_lived_goose_resume_uses_session_name_launch_cwd_and_identity_path() {
    use agman::harness::{self, HarnessKind, LaunchContext, SessionKey};
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let stamped_name = "agman-chief-of-staff-260101-000000-goose123";
    std::fs::write(state_dir.join("session-name"), stamped_name).unwrap();

    let stamped_cwd = tmp.path().join("goose-worktree");
    std::fs::create_dir_all(&stamped_cwd).unwrap();
    std::fs::write(
        state_dir.join("launch-cwd"),
        stamped_cwd.to_string_lossy().as_ref(),
    )
    .unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Goose,
        false,
        None,
    )
    .unwrap();

    assert_eq!(prep.mode, "resume");
    assert!(!prep.is_first_launch);
    assert_eq!(prep.handle.as_deref(), Some(stamped_name));
    assert_eq!(prep.session_name, stamped_name);
    assert_eq!(prep.cwd, stamped_cwd);

    let identity_file = harness::goose::identity_file_path(&state_dir, &prep.session_name);
    let h = HarnessKind::Goose.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Identity body",
        name: &prep.session_name,
        identity_file: Some(&identity_file),
        session_dir: None,
        cwd: &prep.cwd,
        no_alt_screen: false,
        session_key: SessionKey::Resume(&prep.session_name),
    });
    assert!(cmd.contains("GOOSE_MODE=auto"));
    assert!(cmd.contains("GOOSE_MOIM_MESSAGE_FILE="));
    assert!(cmd.contains("--with-builtin developer,tom"));
    assert!(cmd.contains(&format!("--resume --name '{stamped_name}'")));
    assert!(cmd.contains(&identity_file.to_string_lossy().to_string()));
}

#[test]
fn long_lived_pi_first_launch_stamps_name_cwd_session_dir_and_identity() {
    use agman::harness::{self, HarnessKind, LaunchContext};
    use agman::use_cases::{
        prepare_identity_file_for_harness_for_test, prepare_long_lived_launch_for_test,
    };

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    let cwd = tmp.path().join("worktree");
    std::fs::create_dir_all(&cwd).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        &cwd,
        HarnessKind::Pi,
        false,
        None,
    )
    .unwrap();

    assert_eq!(prep.mode, "auto");
    assert!(prep.is_first_launch);
    assert!(prep.handle.is_none());
    assert_eq!(
        std::fs::read_to_string(state_dir.join("session-name"))
            .unwrap()
            .trim(),
        prep.session_name
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("launch-cwd"))
            .unwrap()
            .trim(),
        cwd.to_string_lossy()
    );
    assert_eq!(
        prep.session_dir.as_deref(),
        Some(harness::pi::long_lived_session_dir(&state_dir).as_path())
    );
    assert!(prep.session_dir.as_ref().unwrap().exists());

    let identity_file = prepare_identity_file_for_harness_for_test(
        HarnessKind::Pi,
        &state_dir,
        &prep.session_name,
        "Pi identity body",
        prep.is_first_launch,
    )
    .unwrap()
    .expect("pi uses an identity file");
    assert_eq!(
        std::fs::read_to_string(&identity_file).unwrap(),
        "Pi identity body"
    );

    let h = HarnessKind::Pi.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Pi identity body",
        name: &prep.session_name,
        identity_file: Some(&identity_file),
        session_dir: prep.session_dir.as_deref(),
        cwd: &prep.cwd,
        no_alt_screen: false,
        session_key: agman::harness::SessionKey::Auto,
    });
    assert!(cmd.contains("pi --offline"));
    assert!(cmd.contains("--append-system-prompt "));
    assert!(cmd.contains("--session-dir "));
    assert!(cmd.contains("--tools read,bash,edit,write,grep,find,ls"));
    assert!(!cmd.contains("--continue"));
}

#[test]
fn long_lived_pi_resume_uses_session_dir_continue_and_stamped_cwd() {
    use agman::harness::{self, HarnessKind, LaunchContext, SessionKey};
    use agman::use_cases::{
        prepare_identity_file_for_harness_for_test, prepare_long_lived_launch_for_test,
    };

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let stamped_name = "agman-chief-of-staff-260101-000000-pi123456";
    std::fs::write(state_dir.join("session-name"), stamped_name).unwrap();
    let stamped_cwd = tmp.path().join("pi-worktree");
    std::fs::create_dir_all(&stamped_cwd).unwrap();
    std::fs::write(
        state_dir.join("launch-cwd"),
        stamped_cwd.to_string_lossy().as_ref(),
    )
    .unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Pi,
        false,
        None,
    )
    .unwrap();

    assert_eq!(prep.mode, "resume");
    assert!(!prep.is_first_launch);
    assert_eq!(prep.handle.as_deref(), Some(stamped_name));
    assert_eq!(prep.session_name, stamped_name);
    assert_eq!(prep.cwd, stamped_cwd);
    assert_eq!(
        prep.session_dir.as_deref(),
        Some(harness::pi::long_lived_session_dir(&state_dir).as_path())
    );

    let identity_file = prepare_identity_file_for_harness_for_test(
        HarnessKind::Pi,
        &state_dir,
        &prep.session_name,
        "Fresh Pi identity on resume",
        false,
    )
    .unwrap()
    .expect("pi uses an identity file on resume");
    let h = HarnessKind::Pi.select();
    let cmd = h.build_session_command(&LaunchContext {
        identity: "Fresh Pi identity on resume",
        name: &prep.session_name,
        identity_file: Some(&identity_file),
        session_dir: prep.session_dir.as_deref(),
        cwd: &prep.cwd,
        no_alt_screen: false,
        session_key: SessionKey::Resume(&prep.session_name),
    });
    assert!(cmd.contains("--continue"));
    assert!(cmd.contains("--append-system-prompt "));
    assert!(cmd.contains("--session-dir "));
    assert!(!cmd.contains("--system-prompt"));
    assert!(!cmd.contains("--session-id"));
}

#[test]
fn long_lived_resume_pi_falls_back_when_stamped_cwd_missing() {
    use agman::harness::HarnessKind;
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(
        state_dir.join("session-name"),
        "agman-chief-of-staff-260101-000000-pi999999",
    )
    .unwrap();
    let deleted_cwd = tmp.path().join("deleted-pi-cwd");
    std::fs::create_dir_all(&deleted_cwd).unwrap();
    std::fs::remove_dir_all(&deleted_cwd).unwrap();
    std::fs::write(
        state_dir.join("launch-cwd"),
        deleted_cwd.to_string_lossy().as_ref(),
    )
    .unwrap();

    let live_cwd = tmp.path().join("live-pi-worktree");
    std::fs::create_dir_all(&live_cwd).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        &live_cwd,
        HarnessKind::Pi,
        false,
        None,
    )
    .unwrap();

    assert_eq!(prep.mode, "resume");
    assert_eq!(prep.cwd, live_cwd);
    assert_ne!(prep.cwd, deleted_cwd);
    assert!(prep.session_dir.unwrap().exists());
}

#[test]
fn respawn_wipes_session_handles_then_fresh_launch_mints_new() {
    // Pre-stamp <state_dir>/session-id and <state_dir>/launch-cwd. Run
    // the respawn cleanup, assert both are gone, then run a fresh
    // prepare and assert the next launch is FRESH (new uuid, no resume).
    use agman::harness::HarnessKind;
    use agman::use_cases::{prepare_long_lived_launch_for_test, wipe_long_lived_session_handles};

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("session-id"), "old-stale-uuid").unwrap();
    std::fs::write(state_dir.join("session-name"), "old-stale-name").unwrap();
    std::fs::write(state_dir.join("launch-cwd"), "/old/path").unwrap();

    wipe_long_lived_session_handles(&state_dir);
    assert!(!state_dir.join("session-id").exists());
    assert!(!state_dir.join("session-name").exists());
    assert!(!state_dir.join("launch-cwd").exists());

    // Idempotent on the second call (no panic when files are gone).
    wipe_long_lived_session_handles(&state_dir);

    // Subsequent prepare for claude must mint a fresh uuid (never
    // reuses the stale one).
    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Claude,
        false,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "pin");
    assert!(prep.is_first_launch);
    let new_uuid = prep.handle.unwrap();
    assert_ne!(new_uuid, "old-stale-uuid");
    assert_ne!(prep.session_name, "old-stale-name");
}

#[test]
fn long_lived_force_fresh_ignores_stamped_handle() {
    // force_fresh = true (used by respawn_agent) skips the resume
    // detection on both harnesses. Claude re-mints a UUID; codex falls
    // back to Auto and re-stamps launch-cwd.
    use agman::harness::HarnessKind;
    use agman::use_cases::prepare_long_lived_launch_for_test;

    // --- Claude path ---
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let stale_uuid = "stale-uuid-aaaa-bbbb-cccc-dddddddddddd";
    let stale_name = "agman-chief-of-staff-260101-000000-stale000";
    std::fs::write(state_dir.join("session-id"), stale_uuid).unwrap();
    std::fs::write(state_dir.join("session-name"), stale_name).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Claude,
        true,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "pin");
    assert!(prep.is_first_launch);
    let new_uuid = prep.handle.expect("force_fresh must mint a new uuid");
    assert_ne!(
        new_uuid, stale_uuid,
        "force_fresh must NOT reuse stale uuid"
    );
    assert_ne!(
        prep.session_name, stale_name,
        "force_fresh must NOT reuse stale session-name"
    );

    // --- Codex path ---
    let codex_state = tmp.path().join("codex-ceo");
    std::fs::create_dir_all(&codex_state).unwrap();
    std::fs::write(codex_state.join("launch-cwd"), "/old/path").unwrap();
    std::fs::write(codex_state.join("session-name"), stale_name).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &codex_state,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Codex,
        true,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "auto");
    assert!(prep.is_first_launch);
    assert_ne!(prep.session_name, stale_name);
    let stamped = std::fs::read_to_string(codex_state.join("launch-cwd")).unwrap();
    assert_eq!(stamped.trim(), tmp.path().to_string_lossy());

    // --- Pi path ---
    let pi_state = tmp.path().join("pi-ceo");
    std::fs::create_dir_all(&pi_state).unwrap();
    std::fs::write(pi_state.join("launch-cwd"), "/old/pi/path").unwrap();
    std::fs::write(pi_state.join("session-name"), stale_name).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &pi_state,
        "agman-chief-of-staff",
        tmp.path(),
        HarnessKind::Pi,
        true,
        None,
    )
    .unwrap();
    assert_eq!(prep.mode, "auto");
    assert!(prep.is_first_launch);
    assert_ne!(prep.session_name, stale_name);
    assert_eq!(
        std::fs::read_to_string(pi_state.join("launch-cwd"))
            .unwrap()
            .trim(),
        tmp.path().to_string_lossy()
    );
    assert_eq!(
        prep.session_dir.as_deref(),
        Some(agman::harness::pi::long_lived_session_dir(&pi_state).as_path())
    );
}

#[test]
fn long_lived_resume_codex_falls_back_when_stamped_cwd_missing() {
    // Partial-state robustness: if `<state_dir>/launch-cwd` points at a
    // directory that no longer exists, the resume path must still take the
    // resume branch but fall back to the freshly-resolved `cwd` argument
    // (not pass codex a non-existent directory). This pins the
    // `.filter(|p| p.exists())` fallback in `prepare_long_lived_launch_inner`.
    use agman::harness::HarnessKind;
    use agman::use_cases::prepare_long_lived_launch_for_test;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(
        state_dir.join("session-name"),
        "agman-chief-of-staff-260101-000000-c0decafe",
    )
    .unwrap();

    // Build a path under tmp that exists, then remove it so the stamp
    // points at a guaranteed-missing directory. The enclosing `TempDir`
    // is already unique to this test, so no extra uniqueness is needed.
    let deleted_cwd = tmp.path().join("agman-deleted-launch-cwd");
    std::fs::create_dir_all(&deleted_cwd).unwrap();
    std::fs::remove_dir_all(&deleted_cwd).unwrap();
    assert!(!deleted_cwd.exists());
    std::fs::write(
        state_dir.join("launch-cwd"),
        deleted_cwd.to_string_lossy().as_ref(),
    )
    .unwrap();

    // Pass a real, existing cwd as the freshly-resolved argument.
    let live_cwd = tmp.path().join("live-worktree");
    std::fs::create_dir_all(&live_cwd).unwrap();

    let prep = prepare_long_lived_launch_for_test(
        &state_dir,
        "agman-chief-of-staff",
        &live_cwd,
        HarnessKind::Codex,
        false,
        None,
    )
    .unwrap();

    assert_eq!(prep.mode, "resume", "session-name stamp -> resume path");
    assert_eq!(
        prep.cwd, live_cwd,
        "stamped launch-cwd points at a deleted dir; must fall back to the live cwd argument, not the stamped path"
    );
    assert_ne!(prep.cwd, deleted_cwd);
}

// ---------------------------------------------------------------------------
// Asymmetric harness pinning — long-lived agents pin, task agents don't.
// ---------------------------------------------------------------------------

#[test]
fn respawn_agent_drops_harness_stamp() {
    // Pin: respawn must wipe `<state_dir>/harness` so the next spawn re-reads
    // the current global. Other long-lived state (session-id, launch-cwd)
    // is wiped at the same time. TempDir-isolated; no tmux involved.
    use agman::use_cases::wipe_long_lived_session_handles;

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("ceo");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("harness"), "claude").unwrap();
    std::fs::write(state_dir.join("session-id"), "old-uuid").unwrap();
    std::fs::write(state_dir.join("session-name"), "old-name").unwrap();
    std::fs::write(state_dir.join("launch-cwd"), "/old/path").unwrap();

    wipe_long_lived_session_handles(&state_dir);

    assert!(
        !state_dir.join("harness").exists(),
        "harness stamp must be wiped on respawn so the next spawn re-reads global"
    );
    assert!(!state_dir.join("session-id").exists());
    assert!(!state_dir.join("session-name").exists());
    assert!(!state_dir.join("launch-cwd").exists());
}

#[test]
fn long_lived_harness_pin_survives_global_flip() {
    // Pin: a long-lived agent stamps its harness on first spawn at
    // <state_dir>/harness. Subsequent spawns read the stamped value via
    // `read_or_stamp` regardless of the current global setting — that's
    // the whole point of the pin. To flip a long-lived agent, the user
    // must respawn it (which wipes the stamp).
    use agman::harness::{read_or_stamp, HarnessKind};

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("project-foo");
    std::fs::create_dir_all(&state_dir).unwrap();

    // First spawn under codex: stamps codex.
    let kind1 = read_or_stamp(&state_dir, HarnessKind::Codex).unwrap();
    assert_eq!(kind1, HarnessKind::Codex);
    assert_eq!(
        std::fs::read_to_string(state_dir.join("harness"))
            .unwrap()
            .trim(),
        "codex"
    );

    // Global flips to claude. Next spawn reads the stamped codex back —
    // the global default is ignored when a stamp exists.
    let kind2 = read_or_stamp(&state_dir, HarnessKind::Claude).unwrap();
    assert_eq!(
        kind2,
        HarnessKind::Codex,
        "stamp must override the changed default"
    );
}

#[test]
fn task_meta_session_entry_records_harness_round_trip() {
    // Pin: SessionEntry carries the harness used at spawn so the kill
    // path can dispatch the right slash command. The field must serialize
    // and deserialize cleanly through meta.json.
    use agman::harness::HarnessKind;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    task.push_session(SessionEntry {
        agent: "coder".to_string(),
        name: "agman-task-repo--branch-step-1".to_string(),
        started_at: chrono::Utc::now(),
        stopped_at: None,
        condition: None,
        harness: HarnessKind::Goose,
    })
    .unwrap();

    // Reload from disk to verify the field survives the JSON round-trip.
    let reloaded = Task::load(&config, "repo", "branch").unwrap();
    let last = reloaded.meta.session_history.last().unwrap();
    assert_eq!(last.harness, HarnessKind::Goose);
}

#[test]
fn task_meta_session_entry_records_pi_harness_round_trip() {
    use agman::harness::HarnessKind;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    task.push_session(SessionEntry {
        agent: "coder".to_string(),
        name: "agman-task-repo--branch-step-1".to_string(),
        started_at: chrono::Utc::now(),
        stopped_at: None,
        condition: None,
        harness: HarnessKind::Pi,
    })
    .unwrap();

    let reloaded = Task::load(&config, "repo", "branch").unwrap();
    let last = reloaded.meta.session_history.last().unwrap();
    assert_eq!(last.harness, HarnessKind::Pi);
}

#[test]
fn task_meta_session_entry_legacy_record_defaults_to_claude() {
    // Pin: legacy SessionEntry records (written before SessionEntry.harness
    // existed) deserialize with `harness = HarnessKind::Claude` via
    // `#[serde(default)]`. Documented risk: a stale unstopped codex session
    // entry would read back as Claude and the kill path would dispatch
    // `/exit` (claude) instead of `/quit` (codex), falling through to the
    // Ctrl-C × N fallback. Acceptable; not worth a migration.
    use agman::harness::HarnessKind;

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    // Write a meta.json by hand so we can omit the harness key from the
    // session_history entry. Also include a stray legacy `harness: "codex"`
    // at the TaskMeta root to confirm serde silently drops it.
    let meta_path = task.dir.join("meta.json");
    let raw_meta = serde_json::json!({
        "name": task.meta.name,
        "branch_name": task.meta.branch_name,
        "status": "stopped",
        "repos": task.meta.repos,
        "flow_name": task.meta.flow_name,
        "current_agent": null,
        "flow_step": 0,
        "created_at": task.meta.created_at,
        "updated_at": task.meta.updated_at,
        "session_history": [
            {
                "agent": "coder",
                "name": "agman-task-repo--branch-step-1",
                "started_at": chrono::Utc::now(),
                "stopped_at": null,
                "condition": null,
                // no `harness` field
            }
        ],
        // stray legacy root field — serde must drop it without erroring
        "harness": "codex",
    });
    std::fs::write(&meta_path, serde_json::to_string_pretty(&raw_meta).unwrap()).unwrap();

    let reloaded = Task::load(&config, "repo", "branch").unwrap();
    let last = reloaded.meta.session_history.last().unwrap();
    assert_eq!(
        last.harness,
        HarnessKind::Claude,
        "legacy entry without harness field must default to Claude"
    );
}
