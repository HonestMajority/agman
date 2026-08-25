mod helpers;

use agman::agent_model::{AgentAttachment, AgentKind, AgentRecord};
use agman::inbox;
use agman::task::{Task, TaskMeta};
use agman::use_cases;
use helpers::test_config;
use std::fs;

#[test]
fn task_migration_creates_seeded_engineer_for_active_task_without_one() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let task_dir = config.task_dir("repo", "feature");
    fs::create_dir_all(&task_dir).unwrap();
    let worktree_path = config.worktree_path("repo", "feature");
    fs::create_dir_all(&worktree_path).unwrap();

    let mut meta = TaskMeta::new(
        "repo".to_string(),
        "feature".to_string(),
        worktree_path,
        "new".to_string(),
    );
    meta.project = Some("alpha".to_string());
    let task = Task {
        meta,
        dir: task_dir.clone(),
    };
    task.save_meta().unwrap();
    fs::write(
        task_dir.join("TASK.md"),
        "# Legacy Goal\n\nKeep the old brief as context.\n",
    )
    .unwrap();

    use_cases::migrate_old_tasks(&config);
    use_cases::migrate_old_tasks(&config);

    let agents: Vec<_> = AgentRecord::list_all(&config)
        .unwrap()
        .into_iter()
        .filter(|agent| {
            matches!(agent.meta.kind, AgentKind::Engineer)
                && matches!(
                    &agent.meta.attachment,
                    AgentAttachment::Task { task_id, .. } if task_id == "repo--feature"
                )
        })
        .collect();
    assert_eq!(agents.len(), 1, "migration should be idempotent");
    assert_eq!(agents[0].meta.project, "alpha");

    let messages = inbox::read_messages(&config.agent_inbox("alpha", &agents[0].meta.name))
        .expect("engineer inbox should be readable");
    assert_eq!(messages.len(), 1);
    assert!(messages[0].message.contains("Migration note"));
    assert!(messages[0].message.contains("Legacy Goal"));
    assert!(messages[0]
        .message
        .contains("included only as recovered background"));
}
