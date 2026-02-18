use agman::break_persist;
use std::time::{Duration, Instant};

#[test]
fn save_and_load_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("last_break_reset");
    let max_age = Duration::from_secs(2 * 60 * 60);

    let before = Instant::now();
    break_persist::save_break_reset(&path, &before);

    let loaded = break_persist::load_break_reset(&path, max_age).expect("should load successfully");

    // The loaded Instant should represent roughly the same point in time.
    // Allow up to 1 second tolerance for test execution time.
    let diff = if loaded > before {
        loaded.duration_since(before)
    } else {
        before.duration_since(loaded)
    };
    assert!(
        diff < Duration::from_secs(1),
        "loaded instant differs by {diff:?}"
    );
}

#[test]
fn stale_timestamp_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("last_break_reset");
    let max_age = Duration::from_secs(2 * 60 * 60);

    // Write a timestamp from 3 hours ago (older than max_age of 2 hours)
    let three_hours_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 3 * 60 * 60;
    std::fs::write(&path, three_hours_ago.to_string()).unwrap();

    let result = break_persist::load_break_reset(&path, max_age);
    assert!(result.is_none(), "stale timestamp should return None");
}

#[test]
fn missing_file_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonexistent");
    let max_age = Duration::from_secs(2 * 60 * 60);

    let result = break_persist::load_break_reset(&path, max_age);
    assert!(result.is_none(), "missing file should return None");
}

#[test]
fn corrupt_file_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("last_break_reset");
    let max_age = Duration::from_secs(2 * 60 * 60);

    std::fs::write(&path, "not-a-number").unwrap();

    let result = break_persist::load_break_reset(&path, max_age);
    assert!(result.is_none(), "corrupt file should return None");
}
