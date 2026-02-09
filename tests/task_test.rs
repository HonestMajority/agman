mod helpers;

use agman::task::{Task, TaskMeta, TaskStatus};
use helpers::{create_test_task, test_config};

#[test]
fn task_create_and_load() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let worktree_path = config.worktree_path("myrepo", "feat");
    std::fs::create_dir_all(&worktree_path).unwrap();

    let task = Task::create(&config, "myrepo", "feat", "test desc", "new", worktree_path).unwrap();

    // meta.json should exist
    assert!(task.dir.join("meta.json").exists());

    // Roundtrip via load
    let loaded = Task::load(&config, "myrepo", "feat").unwrap();
    assert_eq!(loaded.meta.repo_name, "myrepo");
    assert_eq!(loaded.meta.branch_name, "feat");
    assert_eq!(loaded.meta.flow_name, "new");
    assert_eq!(loaded.meta.status, TaskStatus::Running);
}

#[test]
fn task_meta_new() {
    let meta = TaskMeta::new(
        "repo".to_string(),
        "branch".to_string(),
        "/tmp/fake".into(),
        "new".to_string(),
    );
    assert_eq!(meta.status, TaskStatus::Running);
    assert_eq!(meta.flow_step, 0);
    assert!(meta.feedback_queue.is_empty());
}

#[test]
fn task_update_status() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    task.update_status(TaskStatus::Stopped).unwrap();

    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.status, TaskStatus::Stopped);
}

#[test]
fn task_advance_flow_step() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    task.advance_flow_step().unwrap();
    task.advance_flow_step().unwrap();

    let loaded = Task::load(&config, "repo", "branch").unwrap();
    assert_eq!(loaded.meta.flow_step, 2);
}

#[test]
fn task_write_and_read_task() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.write_task("# Goal\ntest").unwrap();
    let content = task.read_task().unwrap();
    assert_eq!(content, "# Goal\ntest");
}

#[test]
fn task_write_and_read_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.write_notes("my notes").unwrap();
    let notes = task.read_notes().unwrap();
    assert_eq!(notes, "my notes");
}

#[test]
fn task_feedback_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.write_feedback("fix this").unwrap();
    let fb = task.read_feedback().unwrap();
    assert_eq!(fb, "fix this");

    task.clear_feedback().unwrap();
    let fb = task.read_feedback().unwrap();
    assert!(fb.is_empty());
}

#[test]
fn task_feedback_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let mut task = create_test_task(&config, "repo", "branch");

    task.queue_feedback("a").unwrap();
    task.queue_feedback("b").unwrap();

    assert!(task.has_queued_feedback());
    assert_eq!(task.queued_feedback_count(), 2);

    let first = task.pop_feedback_queue().unwrap();
    assert_eq!(first, Some("a".to_string()));

    let second = task.pop_feedback_queue().unwrap();
    assert_eq!(second, Some("b".to_string()));

    let third = task.pop_feedback_queue().unwrap();
    assert_eq!(third, None);
}

#[test]
fn task_agent_log() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    task.append_agent_log("line 1").unwrap();
    task.append_agent_log("line 2").unwrap();

    let log = task.read_agent_log().unwrap();
    assert!(log.contains("line 1"));
    assert!(log.contains("line 2"));

    let tail = task.read_agent_log_tail(1).unwrap();
    assert_eq!(tail, "line 2");
}

#[test]
fn task_list_all_sorting() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let _t1 = create_test_task(&config, "repo", "running");
    // _t1 is already Running

    let mut t2 = create_test_task(&config, "repo", "stopped");
    t2.update_status(TaskStatus::Stopped).unwrap();

    let mut t3 = create_test_task(&config, "repo", "input");
    t3.update_status(TaskStatus::InputNeeded).unwrap();

    let tasks = Task::list_all(&config).unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0].meta.status, TaskStatus::Running);
    assert_eq!(tasks[1].meta.status, TaskStatus::InputNeeded);
    assert_eq!(tasks[2].meta.status, TaskStatus::Stopped);
}

#[test]
fn task_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    let dir = task.dir.clone();
    assert!(dir.exists());

    task.delete(&config).unwrap();
    assert!(!dir.exists());
}

#[test]
fn task_time_since_update() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let task = create_test_task(&config, "repo", "branch");

    let time_str = task.time_since_update();
    assert_eq!(time_str, "just now");
}
