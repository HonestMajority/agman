mod helpers;

use agman::agent_model::{AgentAttachment, AgentKind, AgentRecord};
use agman::inbox;
use agman::task::{Task, TaskMeta};
use agman::use_cases;
use helpers::test_config;
use std::fs;

/// Seed a directory at `~/.agman/ceo/` with an inbox, then migrate.
/// Assert the new path exists and the old one is gone.
#[test]
fn migration_renames_legacy_ceo_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let legacy = config.base_dir.join("ceo");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("inbox.jsonl"), "{\"seq\":1,\"from\":\"ceo\"}\n").unwrap();
    fs::write(legacy.join("session-id"), "abc-123").unwrap();

    config.ensure_dirs().unwrap();

    let new_dir = config.chief_of_staff_dir();
    assert!(
        new_dir.exists(),
        "chief-of-staff dir should exist after migration"
    );
    assert!(
        new_dir.join("inbox.jsonl").exists(),
        "inbox should have been carried over"
    );
    assert!(
        new_dir.join("session-id").exists(),
        "session-id should have been carried over"
    );
    assert!(!legacy.exists(), "legacy ceo dir should be gone");

    // Idempotency: a second run is a no-op and does not error.
    config.ensure_dirs().unwrap();
    assert!(new_dir.exists());
    assert!(!legacy.exists());
}

/// Legacy `~/.agman/researchers/` should be renamed to
/// `~/.agman/agents/` and each `meta.json` rewritten to the new
/// kind-discriminated agent shape with `kind: Researcher`.
#[test]
fn migration_renames_researchers_to_agents_and_stamps_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    // Seed the legacy `researchers/` dir directly — it lives under base_dir
    // and used to be exposed via `Config::researchers_dir()`.
    let legacy_root = config.base_dir.join("researchers");
    fs::create_dir_all(&legacy_root).unwrap();

    let entry = legacy_root.join("alpha--scout");
    fs::create_dir_all(&entry).unwrap();
    let meta = serde_json::json!({
        "name": "scout",
        "project": "alpha",
        "description": "old researcher",
        "created_at": "2024-01-01T00:00:00Z",
        "updated_at": "2024-01-01T00:00:00Z",
        "status": "running",
        "repo": "myrepo",
        "branch": "main",
        "task_id": null,
    });
    fs::write(
        entry.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();

    config.ensure_dirs().unwrap();

    let agents_dir = config.agents_dir();
    assert!(agents_dir.exists(), "agents dir should exist");
    assert!(
        !legacy_root.exists(),
        "legacy researchers dir should be gone"
    );

    let new_entry = agents_dir.join("alpha--scout");
    assert!(new_entry.exists());

    let migrated_meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(new_entry.join("meta.json")).unwrap()).unwrap();
    let kind = migrated_meta.get("kind").expect("kind should be added");
    assert_eq!(
        kind.get("type").and_then(|v| v.as_str()),
        Some("researcher"),
        "kind.type should be 'researcher'"
    );
    assert_eq!(
        kind.get("repo").and_then(|v| v.as_str()),
        Some("myrepo"),
        "kind.repo should carry over the legacy repo"
    );
    assert_eq!(kind.get("branch").and_then(|v| v.as_str()), Some("main"));
    assert!(kind.get("task_id").map(|v| v.is_null()).unwrap_or(false));

    // Top-level repo/branch/task_id are gone.
    assert!(migrated_meta.get("repo").is_none());
    assert!(migrated_meta.get("branch").is_none());
    assert!(migrated_meta.get("task_id").is_none());

    // Idempotency.
    config.ensure_dirs().unwrap();
    assert!(new_entry.exists());
}

/// Legacy global agent dirs are removed instead of being migrated forward.
#[test]
fn migration_removes_legacy_global_agent_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let agents_dir = config.agents_dir();
    let legacy = agents_dir.join("ceo--scout");
    fs::create_dir_all(&legacy).unwrap();
    let cos = agents_dir.join("chief-of-staff--legacy");
    fs::create_dir_all(&cos).unwrap();
    let kept = agents_dir.join("alpha--kept");
    fs::create_dir_all(&kept).unwrap();
    let meta = serde_json::json!({
        "name": "scout",
        "project": "ceo",
        "description": "old",
        "created_at": "2024-01-01T00:00:00Z",
        "updated_at": "2024-01-01T00:00:00Z",
        "status": "running",
        "kind": { "type": "researcher", "repo": null, "branch": null, "task_id": null },
    });
    fs::write(
        legacy.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();

    config.ensure_dirs().unwrap();

    assert!(!legacy.exists(), "legacy ceo-- dir should be gone");
    assert!(!cos.exists(), "chief-of-staff agent dir should be gone");
    assert!(kept.exists(), "project-scoped agent dir should remain");
}

/// `~/.agman/telegram/current-agent` containing `"ceo"` should be rewritten to
/// `"chief-of-staff"`. Global agent references should be reset there too.
#[test]
fn migration_rewrites_telegram_current_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let path = config.telegram_current_agent_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "ceo").unwrap();

    config.ensure_dirs().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "chief-of-staff",
        "current-agent file should be rewritten"
    );

    // Researcher reference rewrite.
    fs::write(&path, "researcher:ceo--scout").unwrap();
    config.ensure_dirs().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "chief-of-staff",
        "global agent current-agent should reset to CoS"
    );

    fs::write(&path, "operator:chief-of-staff--ops").unwrap();
    config.ensure_dirs().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "chief-of-staff",
        "chief-of-staff agent current-agent should reset to CoS"
    );

    // Unrelated value left alone.
    fs::write(&path, "some-project").unwrap();
    config.ensure_dirs().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "some-project");
}

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
