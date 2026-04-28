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
    assert!(new_dir.exists(), "chief-of-staff dir should exist after migration");
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

/// Researcher dir `ceo--<name>` should be renamed and the meta.json `project`
/// field rewritten.
#[test]
fn migration_renames_researcher_dirs_and_rewrites_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let researchers_dir = config.researchers_dir();
    let legacy = researchers_dir.join("ceo--scout");
    fs::create_dir_all(&legacy).unwrap();
    let meta = serde_json::json!({
        "name": "scout",
        "project": "ceo",
        "description": "old researcher",
        "created_at": "2024-01-01T00:00:00Z",
        "updated_at": "2024-01-01T00:00:00Z",
        "status": "running",
        "repo": null,
        "branch": null,
        "task_id": null,
    });
    fs::write(legacy.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();

    config.ensure_dirs().unwrap();

    let new_dir = researchers_dir.join("chief-of-staff--scout");
    assert!(new_dir.exists(), "renamed researcher dir should exist");
    assert!(!legacy.exists(), "legacy researcher dir should be gone");

    let migrated_meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(new_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(
        migrated_meta.get("project").and_then(|v| v.as_str()),
        Some("chief-of-staff"),
        "project field should be rewritten"
    );

    // Idempotency.
    config.ensure_dirs().unwrap();
    assert!(new_dir.exists());
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
