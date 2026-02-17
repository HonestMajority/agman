use agman::dismissed_notifications::DismissedNotifications;

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
    dn.insert("thread-1".to_string());
    dn.insert("thread-2".to_string());
    dn.save(&path);

    let loaded = DismissedNotifications::load(&path);
    assert_eq!(loaded.ids.len(), 2);
    assert!(loaded.contains("thread-1"));
    assert!(loaded.contains("thread-2"));
}

#[test]
fn remove_and_contains() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dismissed.json");

    let mut dn = DismissedNotifications::load(&path);
    dn.insert("thread-1".to_string());
    dn.insert("thread-2".to_string());
    assert!(dn.contains("thread-1"));

    dn.remove("thread-1");
    assert!(!dn.contains("thread-1"));
    assert!(dn.contains("thread-2"));

    dn.save(&path);
    let loaded = DismissedNotifications::load(&path);
    assert!(!loaded.contains("thread-1"));
    assert!(loaded.contains("thread-2"));
}
