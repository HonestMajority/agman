mod helpers;

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
/// `~/.agman/assistants/` and each `meta.json` rewritten to the new
/// kind-discriminated `AssistantMeta` shape with `kind: Researcher`.
#[test]
fn migration_renames_researchers_to_assistants_and_stamps_kind() {
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

    let assistants_dir = config.assistants_dir();
    assert!(assistants_dir.exists(), "assistants dir should exist");
    assert!(!legacy_root.exists(), "legacy researchers dir should be gone");

    let new_entry = assistants_dir.join("alpha--scout");
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
    assert_eq!(
        kind.get("branch").and_then(|v| v.as_str()),
        Some("main")
    );
    assert!(kind.get("task_id").map(|v| v.is_null()).unwrap_or(false));

    // Top-level repo/branch/task_id are gone.
    assert!(migrated_meta.get("repo").is_none());
    assert!(migrated_meta.get("branch").is_none());
    assert!(migrated_meta.get("task_id").is_none());

    // Idempotency.
    config.ensure_dirs().unwrap();
    assert!(new_entry.exists());
}

/// Legacy `ceo--<name>` directory inside the new assistants dir should still
/// be renamed to `chief-of-staff--<name>` (very old installs may have skipped
/// the CoS rename release).
#[test]
fn migration_renames_legacy_ceo_assistant_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let assistants_dir = config.assistants_dir();
    let legacy = assistants_dir.join("ceo--scout");
    fs::create_dir_all(&legacy).unwrap();
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

    let new_dir = assistants_dir.join("chief-of-staff--scout");
    assert!(new_dir.exists(), "renamed assistant dir should exist");
    assert!(!legacy.exists(), "legacy ceo-- dir should be gone");

    let migrated_meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(new_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(
        migrated_meta.get("project").and_then(|v| v.as_str()),
        Some("chief-of-staff"),
    );
}

/// `~/.agman/telegram/current-agent` containing `"ceo"` should be rewritten to
/// `"chief-of-staff"`. A `researcher:ceo--<name>` reference should likewise be
/// rewritten.
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
        "researcher:chief-of-staff--scout"
    );

    // Unrelated value left alone.
    fs::write(&path, "some-project").unwrap();
    config.ensure_dirs().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "some-project");
}
