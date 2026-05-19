mod helpers;

use agman::config::Config;
use agman::supervisor;
use agman::tmux::Tmux;
use agman::use_cases;
use helpers::{create_test_researcher, create_test_task, test_config};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TmuxCleanup {
    sessions: Vec<String>,
}

impl Drop for TmuxCleanup {
    fn drop(&mut self) {
        for session in &self.sessions {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", session])
                .output();
        }
    }
}

#[test]
fn ensure_task_tmux_backfills_attached_agent_windows_after_creation_and_recreation() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping tmux backfill smoke: tmux binary is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let unique = unique_test_name();
    let project = format!("repo-{unique}");
    let branch = format!("branch-{unique}");
    let task = create_test_task(&config, &project, &branch);
    let task_id = task.meta.task_id();
    let researcher = create_test_researcher(
        &config,
        &project,
        "researcher-agent-window-names-and-selection",
    );
    use_cases::attach_agent_to_task(&config, &project, &researcher.meta.name, &task_id, None)
        .unwrap();

    let engineer = use_cases::attached_engineer_for_task(&config, &task_id).unwrap();
    let engineer_session = Config::engineer_tmux_session(&project, &engineer.meta.name);
    let researcher_session = Config::researcher_tmux_session(&project, &researcher.meta.name);
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let cleanup_sessions = vec![
        task_session.clone(),
        engineer_session.clone(),
        researcher_session.clone(),
    ];
    let _cleanup = TmuxCleanup {
        sessions: cleanup_sessions,
    };

    create_tmux_session(&engineer_session, tmp.path());
    create_tmux_session(&researcher_session, tmp.path());

    let engineer_window =
        Tmux::linked_agent_window_name("engineer", &engineer.meta.name, &engineer_session);
    let researcher_window =
        Tmux::linked_agent_window_name("researcher", &researcher.meta.name, &researcher_session);

    supervisor::ensure_task_tmux(&config, &task).unwrap();
    let first_windows = window_names(&task_session);
    assert!(first_windows.contains(&engineer_window));
    assert!(first_windows.contains(&researcher_window));
    assert_eq!(
        window_names(&engineer_session),
        vec![engineer_window.clone()]
    );
    assert_eq!(
        window_names(&researcher_session),
        vec![researcher_window.clone()]
    );
    assert_compact_agent_windows(&first_windows);

    Command::new("tmux")
        .args(["kill-session", "-t", &task_session])
        .output()
        .unwrap();

    supervisor::ensure_task_tmux(&config, &task).unwrap();
    let recreated_windows = window_names(&task_session);
    assert!(recreated_windows.contains(&engineer_window));
    assert!(recreated_windows.contains(&researcher_window));
    assert_compact_agent_windows(&recreated_windows);
}

fn unique_test_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn create_tmux_session(session: &str, cwd: &std::path::Path) {
    let output = Command::new("tmux")
        .args(["new-session", "-d", "-s", session, "-n", "main", "-c"])
        .arg(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to create tmux session {session}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn window_names(session: &str) -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to list tmux windows for {session}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn assert_compact_agent_windows(windows: &[String]) {
    let agent_windows: Vec<_> = windows
        .iter()
        .filter(|name| {
            name.starts_with("Engineer-")
                || name.starts_with("Researcher-")
                || name.starts_with("engineer-")
                || name.starts_with("researcher-")
        })
        .collect();
    assert!(!agent_windows.is_empty(), "no agent windows in {windows:?}");
    for name in agent_windows {
        assert!(name.len() <= 24, "agent window label is too long: {name}");
        assert!(
            name.starts_with("Engineer-") || name.starts_with("Researcher-"),
            "agent window label kept old lowercase form: {name}"
        );
    }
}
