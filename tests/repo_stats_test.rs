use agman::repo_stats::RepoStats;

#[test]
fn repo_stats_load_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonexistent.json");

    let stats = RepoStats::load(&path);
    assert!(stats.counts.is_empty());
}

#[test]
fn repo_stats_increment_and_save_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("stats.json");

    let mut stats = RepoStats::load(&path);
    stats.increment("myrepo");
    stats.increment("myrepo");
    stats.increment("myrepo");
    stats.save(&path);

    let loaded = RepoStats::load(&path);
    assert_eq!(loaded.counts.get("myrepo"), Some(&3));
}

#[test]
fn repo_stats_favorites_sorted() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("stats.json");

    let mut stats = RepoStats::load(&path);
    stats.increment("a");
    stats.increment("b");
    stats.increment("b");
    stats.increment("b");
    stats.increment("c");
    stats.increment("c");

    let favs = stats.favorites();
    assert_eq!(favs.len(), 3);
    assert_eq!(favs[0], ("b".to_string(), 3));
    assert_eq!(favs[1], ("c".to_string(), 2));
    assert_eq!(favs[2], ("a".to_string(), 1));
}
