use agman::dismissed_notifications::DismissedNotifications;
use chrono::{Duration, Utc};

#[test]
fn load_missing_file_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonexistent.json");

    let dn = DismissedNotifications::load(&path);
    assert!(dn.ids.is_empty());
}

#[test]
fn insert_save_load_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dismissed.json");

    let mut dn = DismissedNotifications::load(&path);
    dn.insert("thread-1".to_string(), "2025-01-01T00:00:00Z".to_string());
    dn.insert("thread-2".to_string(), "2025-01-02T00:00:00Z".to_string());
    dn.save(&path);

    let loaded = DismissedNotifications::load(&path);
    assert_eq!(loaded.ids.len(), 2);
    assert!(loaded.contains("thread-1"));
    assert!(loaded.contains("thread-2"));
    // Verify the updated_at is preserved
    assert_eq!(loaded.ids["thread-1"].updated_at, "2025-01-01T00:00:00Z");
    assert_eq!(loaded.ids["thread-2"].updated_at, "2025-01-02T00:00:00Z");
}

#[test]
fn remove_and_contains() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dismissed.json");

    let mut dn = DismissedNotifications::load(&path);
    dn.insert("thread-1".to_string(), "2025-01-01T00:00:00Z".to_string());
    dn.insert("thread-2".to_string(), "2025-01-02T00:00:00Z".to_string());
    assert!(dn.contains("thread-1"));

    dn.remove("thread-1");
    assert!(!dn.contains("thread-1"));
    assert!(dn.contains("thread-2"));

    dn.save(&path);
    let loaded = DismissedNotifications::load(&path);
    assert!(!loaded.contains("thread-1"));
    assert!(loaded.contains("thread-2"));
}

#[test]
fn backwards_compatible_load_from_legacy_vec_format() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dismissed.json");

    // Write the old HashSet format: { "ids": ["id1", "id2"] }
    let legacy_json = r#"{"ids":["thread-old-1","thread-old-2"]}"#;
    std::fs::write(&path, legacy_json).unwrap();

    let loaded = DismissedNotifications::load(&path);
    assert_eq!(loaded.ids.len(), 2);
    assert!(loaded.contains("thread-old-1"));
    assert!(loaded.contains("thread-old-2"));
}

#[test]
fn backwards_compatible_load_from_legacy_map_format() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dismissed.json");

    // Write the v2 HashMap<String, String> format: { "ids": { "id": "dismissed_at" } }
    let legacy_json = r#"{"ids":{"thread-1":"2025-06-01T00:00:00Z","thread-2":"2025-06-02T00:00:00Z"}}"#;
    std::fs::write(&path, legacy_json).unwrap();

    let loaded = DismissedNotifications::load(&path);
    assert_eq!(loaded.ids.len(), 2);
    assert!(loaded.contains("thread-1"));
    assert!(loaded.contains("thread-2"));
    // Legacy map entries use dismissed_at as updated_at
    assert_eq!(loaded.ids["thread-1"].dismissed_at, "2025-06-01T00:00:00Z");
    assert_eq!(loaded.ids["thread-1"].updated_at, "2025-06-01T00:00:00Z");
}

#[test]
fn prune_older_than_removes_old_entries() {
    let mut dn = DismissedNotifications {
        ids: std::collections::HashMap::new(),
    };

    // Insert an entry with a timestamp from 4 weeks ago
    let four_weeks_ago = (Utc::now() - Duration::weeks(4)).to_rfc3339();
    dn.ids.insert(
        "old-thread".to_string(),
        agman::dismissed_notifications::DismissedEntry {
            dismissed_at: four_weeks_ago,
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        },
    );

    // Insert a recent entry
    dn.insert("recent-thread".to_string(), "2025-06-01T00:00:00Z".to_string());

    assert_eq!(dn.ids.len(), 2);

    let pruned = dn.prune_older_than(Duration::weeks(3));

    assert_eq!(pruned, 1);
    assert_eq!(dn.ids.len(), 1);
    assert!(!dn.contains("old-thread"));
    assert!(dn.contains("recent-thread"));
}

#[test]
fn prune_older_than_keeps_all_when_none_expired() {
    let mut dn = DismissedNotifications {
        ids: std::collections::HashMap::new(),
    };

    dn.insert("thread-1".to_string(), "2025-06-01T00:00:00Z".to_string());
    dn.insert("thread-2".to_string(), "2025-06-02T00:00:00Z".to_string());

    let pruned = dn.prune_older_than(Duration::weeks(3));

    assert_eq!(pruned, 0);
    assert_eq!(dn.ids.len(), 2);
}

#[test]
fn should_undismiss_when_notification_has_new_activity() {
    let mut dn = DismissedNotifications {
        ids: std::collections::HashMap::new(),
    };

    // Dismiss a notification with updated_at = T1
    dn.insert("thread-1".to_string(), "2025-06-01T10:00:00Z".to_string());

    // Same updated_at — should NOT un-dismiss
    assert!(!dn.should_undismiss("thread-1", "2025-06-01T10:00:00Z"));

    // Older updated_at — should NOT un-dismiss
    assert!(!dn.should_undismiss("thread-1", "2025-06-01T09:00:00Z"));

    // Newer updated_at — SHOULD un-dismiss (new activity)
    assert!(dn.should_undismiss("thread-1", "2025-06-01T11:00:00Z"));

    // Unknown thread — should NOT un-dismiss
    assert!(!dn.should_undismiss("thread-unknown", "2025-06-01T11:00:00Z"));
}
