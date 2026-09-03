use agman::last_viewed::LastViewed;
use std::collections::HashSet;

#[test]
fn load_missing_file_starts_tracking_now() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonexistent.json");

    let lv = LastViewed::load(&path, 1_000);

    assert!(lv.sessions.is_empty());
    assert_eq!(lv.tracking_since, 1_000);
    assert_eq!(lv.epoch_for("never-seen"), 1_000);
}

#[test]
fn load_corrupt_file_starts_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("last_viewed.json");
    std::fs::write(&path, "{not json").unwrap();

    let lv = LastViewed::load(&path, 42);

    assert!(lv.sessions.is_empty());
    assert_eq!(lv.tracking_since, 42);
}

#[test]
fn stamp_save_load_roundtrip_preserves_tracking_since() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("last_viewed.json");

    let mut lv = LastViewed::load(&path, 1_000);
    assert!(lv.stamp("agman-pm-alpha", 2_000));
    assert!(lv.stamp("agman-chief-of-staff", 3_000));
    lv.save(&path);

    let loaded = LastViewed::load(&path, 9_999);
    assert_eq!(loaded, lv);
    assert_eq!(loaded.tracking_since, 1_000);
    assert_eq!(loaded.epoch_for("agman-pm-alpha"), 2_000);
    assert_eq!(loaded.epoch_for("unknown"), 1_000);
}

#[test]
fn stamp_is_monotonic() {
    let mut lv = LastViewed::load(std::path::Path::new("/nonexistent"), 0);

    assert!(lv.stamp("s", 500));
    assert!(!lv.stamp("s", 400));
    assert!(!lv.stamp("s", 500));
    assert!(lv.stamp("s", 501));
    assert_eq!(lv.epoch_for("s"), 501);
}

#[test]
fn retain_sessions_prunes_absent_entries() {
    let mut lv = LastViewed::load(std::path::Path::new("/nonexistent"), 0);
    lv.stamp("keep", 10);
    lv.stamp("drop", 20);

    let roster: HashSet<String> = ["keep".to_string(), "not-yet-stamped".to_string()]
        .into_iter()
        .collect();
    assert_eq!(lv.retain_sessions(&roster), 1);

    assert_eq!(lv.epoch_for("keep"), 10);
    assert_eq!(lv.epoch_for("drop"), lv.tracking_since);
    assert_eq!(lv.retain_sessions(&roster), 0);
}
