mod helpers;

use agman::config::Config;
use agman::harness::HarnessKind;
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
fn config_worktree_path_with_slash() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let path = config.worktree_path("myrepo", "feat/foo");
    assert_eq!(path, tmp.path().join("repos/myrepo-wt/feat-foo"));
}

#[test]
fn config_task_id_with_slash() {
    assert_eq!(Config::task_id("repo", "feat/foo"), "repo--feat-foo");
}

#[test]
fn config_tmux_session_name_with_slash() {
    assert_eq!(
        Config::tmux_session_name("repo", "feat/foo"),
        "(repo)__feat-foo"
    );
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
fn config_tmux_session_name_with_dots() {
    assert_eq!(
        Config::tmux_session_name("repo", "chore-job-0.6.18"),
        "(repo)__chore-job-0_6_18"
    );
}

#[test]
fn config_tmux_session_name_with_slash_and_dots() {
    assert_eq!(
        Config::tmux_session_name("repo", "feat/v1.2.3"),
        "(repo)__feat-v1_2_3"
    );
}

#[test]
fn config_tmux_session_name_with_colon() {
    assert_eq!(
        Config::tmux_session_name("repo", "fix:colon"),
        "(repo)__fix_colon"
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
fn config_accepts_and_persists_goose_harness() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    agman::use_cases::save_harness(&config, HarnessKind::Goose).unwrap();

    assert_eq!(config.harness_kind(), HarnessKind::Goose);
    let raw = std::fs::read_to_string(config.base_dir.join("config.toml")).unwrap();
    assert!(raw.contains("harness = \"goose\""));
}

#[test]
fn config_accepts_and_persists_pi_harness() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    agman::use_cases::save_harness(&config, HarnessKind::Pi).unwrap();

    assert_eq!(config.harness_kind(), HarnessKind::Pi);
    let raw = std::fs::read_to_string(config.base_dir.join("config.toml")).unwrap();
    assert!(raw.contains("harness = \"pi\""));
}

#[test]
fn config_telegram_current_agent_path() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let path = config.telegram_current_agent_path();
    assert_eq!(path, tmp.path().join(".agman/telegram/current-agent"));
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
    let coder = config.prompt_path("coder");
    assert!(coder.exists());
    assert!(!std::fs::read_to_string(&coder).unwrap().is_empty());

    // Verify representative command files exist
    let create_pr = config.command_path("create-pr");
    assert!(create_pr.exists());
    assert!(!std::fs::read_to_string(&create_pr).unwrap().is_empty());
}
