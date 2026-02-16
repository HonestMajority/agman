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
