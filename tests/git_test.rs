mod helpers;

use agman::git::Git;
use helpers::{init_test_repo, test_config};

#[test]
fn git_create_and_remove_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let worktree_path = Git::create_worktree_quiet(&config, "myrepo", "feat-branch", None).unwrap();

    // Worktree directory should exist and contain files
    assert!(worktree_path.exists());
    assert!(worktree_path.join("README.md").exists());

    // Remove it
    let repo_path = config.repo_path("myrepo");
    Git::remove_worktree(&repo_path, &worktree_path).unwrap();
    assert!(!worktree_path.exists());
}

#[test]
fn git_list_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo_path = init_test_repo(&tmp, "myrepo");

    Git::create_worktree_quiet(&config, "myrepo", "test-branch", None).unwrap();

    let worktrees = Git::list_worktrees(&repo_path).unwrap();
    let branches: Vec<&str> = worktrees.iter().map(|(b, _p)| b.as_str()).collect();
    assert!(branches.contains(&"main"));
    assert!(branches.contains(&"test-branch"));
}

#[test]
fn git_delete_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo_path = init_test_repo(&tmp, "myrepo");

    let worktree_path = Git::create_worktree_quiet(&config, "myrepo", "to-delete", None).unwrap();
    Git::remove_worktree(&repo_path, &worktree_path).unwrap();

    Git::delete_branch(&repo_path, "to-delete").unwrap();

    // Verify the branch no longer exists
    let output = std::process::Command::new("git")
        .args(["branch", "--list", "to-delete"])
        .current_dir(&repo_path)
        .output()
        .unwrap();
    let branches = String::from_utf8_lossy(&output.stdout);
    assert!(!branches.contains("to-delete"));
}

#[test]
fn git_create_worktree_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    let path1 = Git::create_worktree_quiet(&config, "myrepo", "idem-branch", None).unwrap();
    assert!(path1.exists());

    // Calling again should succeed and return the same path
    let path2 = Git::create_worktree_quiet(&config, "myrepo", "idem-branch", None).unwrap();
    assert_eq!(path1, path2);
    assert!(path2.exists());
}

#[test]
fn git_create_worktree_with_dangling_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo_path = init_test_repo(&tmp, "myrepo");

    // Create a worktree (this also creates the local branch)
    let worktree_path = Git::create_worktree_quiet(&config, "myrepo", "dangling-branch", None).unwrap();
    assert!(worktree_path.exists());

    // Simulate a dangling branch: remove the worktree directory but keep the branch.
    // First, unregister the worktree from git so git doesn't complain about it.
    let _ = std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["worktree", "remove", "--force", worktree_path.to_str().unwrap()])
        .output()
        .unwrap();
    // The worktree directory is gone but the local branch still exists
    assert!(!worktree_path.exists());
    let branch_check = std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["rev-parse", "--verify", "refs/heads/dangling-branch"])
        .output()
        .unwrap();
    assert!(branch_check.status.success(), "local branch should still exist");

    // Now retry creating the worktree with the same branch name — this should succeed
    // thanks to -B (would fail with -b because the branch already exists)
    let worktree_path2 = Git::create_worktree_quiet(&config, "myrepo", "dangling-branch", None).unwrap();
    assert!(worktree_path2.exists());
    assert!(worktree_path2.join("README.md").exists());
}

#[test]
fn git_create_worktree_for_existing_branch_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let _repo_path = init_test_repo(&tmp, "myrepo");

    // Create a worktree (this also creates the branch)
    let path1 = Git::create_worktree_quiet(&config, "myrepo", "exist-branch", None).unwrap();
    assert!(path1.exists());

    // Now call create_worktree_for_existing_branch_quiet for the same branch —
    // the worktree is already on disk, so it should reuse it
    let path2 =
        Git::create_worktree_for_existing_branch_quiet(&config, "myrepo", "exist-branch").unwrap();
    assert_eq!(path1, path2);
    assert!(path2.exists());
}

#[test]
fn git_create_worktree_with_custom_base_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let repo_path = init_test_repo(&tmp, "myrepo");

    // Create a side branch with a unique commit
    std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["checkout", "-b", "side-branch"])
        .output()
        .unwrap();
    std::fs::write(repo_path.join("side.txt"), "side content").unwrap();
    std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["add", "side.txt"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["commit", "-m", "side commit"])
        .output()
        .unwrap();

    // Get the commit SHA for the side branch
    let side_sha = std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["rev-parse", "side-branch"])
        .output()
        .unwrap();
    let side_sha = String::from_utf8_lossy(&side_sha.stdout).trim().to_string();

    // Go back to main
    std::process::Command::new("git")
        .current_dir(&repo_path)
        .args(["checkout", "main"])
        .output()
        .unwrap();

    // Create a worktree with a custom base ref pointing at the side branch
    let worktree_path = Git::create_worktree_quiet(
        &config,
        "myrepo",
        "new-from-side",
        Some("side-branch"),
    )
    .unwrap();

    assert!(worktree_path.exists());

    // The new branch should be at the same commit as the side branch
    let wt_sha = std::process::Command::new("git")
        .current_dir(&worktree_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let wt_sha = String::from_utf8_lossy(&wt_sha.stdout).trim().to_string();
    assert_eq!(wt_sha, side_sha);

    // side.txt should be present (inherited from the side branch)
    assert!(worktree_path.join("side.txt").exists());
}
