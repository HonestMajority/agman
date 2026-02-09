mod helpers;

use agman::config::Config;
use helpers::test_config;

#[test]
fn config_new_sets_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    assert_eq!(config.base_dir, tmp.path().join(".agman"));
    assert_eq!(config.tasks_dir, tmp.path().join(".agman/tasks"));
    assert_eq!(config.flows_dir, tmp.path().join(".agman/flows"));
    assert_eq!(config.prompts_dir, tmp.path().join(".agman/prompts"));
    assert_eq!(config.commands_dir, tmp.path().join(".agman/commands"));
    assert_eq!(config.repos_dir, tmp.path().join("repos"));
}

#[test]
fn config_task_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let dir = config.task_dir("myrepo", "feat");
    assert_eq!(dir, tmp.path().join(".agman/tasks/myrepo--feat"));
}

#[test]
fn config_worktree_path() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let path = config.worktree_path("myrepo", "feat");
    assert_eq!(path, tmp.path().join("repos/myrepo-wt/feat"));
}

#[test]
fn config_task_id_and_parse() {
    assert_eq!(Config::task_id("repo", "branch"), "repo--branch");

    let parsed = Config::parse_task_id("repo--branch");
    assert_eq!(parsed, Some(("repo".to_string(), "branch".to_string())));

    assert_eq!(Config::parse_task_id("no-separator"), None);
}

#[test]
fn config_tmux_session_name() {
    assert_eq!(
        Config::tmux_session_name("repo", "branch"),
        "(repo)__branch"
    );
}

#[test]
fn config_ensure_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    config.ensure_dirs().unwrap();

    assert!(config.tasks_dir.exists());
    assert!(config.flows_dir.exists());
    assert!(config.prompts_dir.exists());
    assert!(config.commands_dir.exists());
}

#[test]
fn config_init_default_files() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    config.init_default_files(false).unwrap();

    // Verify representative flow files exist and are non-empty
    let new_flow = config.flow_path("new");
    assert!(new_flow.exists());
    assert!(!std::fs::read_to_string(&new_flow).unwrap().is_empty());

    let continue_flow = config.flow_path("continue");
    assert!(continue_flow.exists());

    // Verify representative prompt files exist
    let planner = config.prompt_path("planner");
    assert!(planner.exists());
    assert!(!std::fs::read_to_string(&planner).unwrap().is_empty());

    let coder = config.prompt_path("coder");
    assert!(coder.exists());

    // Verify representative command files exist
    let create_pr = config.command_path("create-pr");
    assert!(create_pr.exists());
    assert!(!std::fs::read_to_string(&create_pr).unwrap().is_empty());
}
