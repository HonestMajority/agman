use agman::config::Config;
use agman::project::Project;
use agman::task::{Task, TaskMeta};
use std::path::PathBuf;
use tempfile::TempDir;

/// Build a Config rooted in the temp dir.
#[allow(dead_code)]
pub fn test_config(tmp: &TempDir) -> Config {
    let base_dir = tmp.path().join(".agman");
    let repos_dir = tmp.path().join("repos");
    Config::new(base_dir, repos_dir)
}

/// Create a bare git repo at `<repos>/<name>/` with an initial commit.
#[allow(dead_code)]
pub fn init_test_repo(tmp: &TempDir, name: &str) -> PathBuf {
    let repo_path = tmp.path().join("repos").join(name);
    std::fs::create_dir_all(&repo_path).unwrap();

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo_path)
            .output()
            .expect("git command failed")
    };

    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "Test"]);

    // Create an initial commit so HEAD exists
    std::fs::write(repo_path.join("README.md"), "# test repo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial commit"]);

    repo_path
}

/// Create a git repo at `<dir>/<name>/` with an initial commit.
/// Unlike `init_test_repo`, allows placing the repo at an arbitrary path
/// (not necessarily under `repos/`). Used for multi-repo task tests.
#[allow(dead_code)]
pub fn init_test_repo_at(dir: &std::path::Path, name: &str) -> PathBuf {
    let repo_path = dir.join(name);
    std::fs::create_dir_all(&repo_path).unwrap();

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo_path)
            .output()
            .expect("git command failed")
    };

    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@test.com"]);
    run(&["config", "user.name", "Test"]);

    std::fs::write(repo_path.join("README.md"), "# test repo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "initial commit"]);

    repo_path
}

/// Create a minimal Task (directory + meta.json + init files) without real git.
/// Sets up a fake worktree directory so file I/O methods work.
#[allow(dead_code)]
pub fn create_test_task(config: &Config, repo_name: &str, branch_name: &str) -> Task {
    config.ensure_dirs().unwrap();

    let worktree_path = config.worktree_path(repo_name, branch_name);
    std::fs::create_dir_all(&worktree_path).unwrap();

    let dir = config.task_dir(repo_name, branch_name);
    std::fs::create_dir_all(&dir).unwrap();

    let meta = TaskMeta::new(
        repo_name.to_string(),
        branch_name.to_string(),
        worktree_path,
        "new".to_string(),
    );

    let task = Task { meta, dir };
    task.save_meta().unwrap();
    // Create the same init files that Task::create() makes
    for file in ["notes.md", "agent.log"] {
        let path = task.dir.join(file);
        if !path.exists() {
            std::fs::write(&path, "").unwrap();
        }
    }

    task
}

/// Create a minimal Project for testing.
#[allow(dead_code)]
pub fn create_test_project(config: &Config, name: &str) -> Project {
    config.ensure_dirs().unwrap();
    Project::create(config, name, &format!("Test project {name}")).unwrap()
}
