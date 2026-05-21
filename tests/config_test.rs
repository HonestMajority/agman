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
    assert_eq!(config.prompts_dir, tmp.path().join(".agman/prompts"));
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
fn config_project_notes_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);

    let dir = config.project_notes_dir("my-project");
    assert_eq!(dir, tmp.path().join(".agman/projects/my-project/notes"));
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
    assert!(config.prompts_dir.exists());
    assert!(config.agents_dir().exists());
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

    let engineer = config.prompt_path("engineer");
    assert!(engineer.exists());
    assert!(std::fs::read_to_string(&engineer)
        .unwrap()
        .contains("long-lived task-attached engineer"));
}
