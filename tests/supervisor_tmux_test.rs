mod helpers;

use agman::agent_model::{AgentAttachment, AgentStatus};
use agman::config::Config;
use agman::supervisor;
use agman::tmux::Tmux;
use agman::use_cases;
use helpers::{create_test_project, create_test_researcher, create_test_task, test_config};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
fn archive_task_kills_task_sessions_and_attached_agent_sessions() {
    if !tmux_available() {
        eprintln!("skipping tmux lifecycle smoke: tmux binary is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let unique = unique_test_name();
    let project = format!("repo-{unique}");
    let branch = format!("branch-{unique}");
    let mut task = create_test_task(&config, &project, &branch);
    let task_id = task.meta.task_id();
    let researcher = create_test_researcher(&config, &project, &format!("research-{unique}"));
    use_cases::attach_agent_to_task(&config, &project, &researcher.meta.name, &task_id, None)
        .unwrap();
    let engineer = use_cases::attached_engineer_for_task(&config, &task_id).unwrap();
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let engineer_session = Config::engineer_tmux_session(&project, &engineer.meta.name);
    let researcher_session = Config::researcher_tmux_session(&project, &researcher.meta.name);
    let _cleanup = TmuxCleanup {
        sessions: vec![
            task_session.clone(),
            engineer_session.clone(),
            researcher_session.clone(),
        ],
    };
    create_tmux_session(&task_session, tmp.path());
    create_tmux_session(&engineer_session, tmp.path());
    create_tmux_session(&researcher_session, tmp.path());

    use_cases::archive_task(&config, &mut task, false).unwrap();

    assert!(!Tmux::session_exists(&task_session));
    assert!(!Tmux::session_exists(&engineer_session));
    assert!(!Tmux::session_exists(&researcher_session));
}

#[test]
fn archive_agent_kills_canonical_session_and_unlinks_task_window() {
    if !tmux_available() {
        eprintln!("skipping tmux lifecycle smoke: tmux binary is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let unique = unique_test_name();
    let project = format!("repo-{unique}");
    let branch = format!("branch-{unique}");
    let task = create_test_task(&config, &project, &branch);
    let task_id = task.meta.task_id();
    let researcher = create_test_researcher(&config, &project, &format!("research-{unique}"));
    use_cases::attach_agent_to_task(&config, &project, &researcher.meta.name, &task_id, None)
        .unwrap();
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let researcher_session = Config::researcher_tmux_session(&project, &researcher.meta.name);
    let window_name =
        Tmux::linked_agent_window_name("researcher", &researcher.meta.name, &researcher_session);
    let _cleanup = TmuxCleanup {
        sessions: vec![task_session.clone(), researcher_session.clone()],
    };
    create_tmux_session(&task_session, tmp.path());
    create_tmux_session(&researcher_session, tmp.path());
    Tmux::link_agent_window(&task_session, &researcher_session, &window_name).unwrap();
    assert!(window_names(&task_session).contains(&window_name));

    use_cases::archive_agent(&config, &project, &researcher.meta.name).unwrap();

    assert!(Tmux::session_exists(&task_session));
    assert!(!Tmux::session_exists(&researcher_session));
    assert!(!window_names(&task_session).contains(&window_name));
}

#[test]
fn delete_project_kills_pm_task_and_agent_sessions() {
    if !tmux_available() {
        eprintln!("skipping tmux lifecycle smoke: tmux binary is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let unique = unique_test_name();
    let project = format!("repo-{unique}");
    let branch = format!("branch-{unique}");
    create_test_project(&config, &project);
    let task = create_test_task(&config, &project, &branch);
    let task_id = task.meta.task_id();
    let engineer = use_cases::attached_engineer_for_task(&config, &task_id).unwrap();
    let running_agent = create_test_researcher(&config, &project, &format!("running-{unique}"));
    let mut archived_agent =
        create_test_researcher(&config, &project, &format!("archived-{unique}"));
    archived_agent.meta.status = AgentStatus::Archived;
    archived_agent.meta.attachment = AgentAttachment::Unattached;
    archived_agent.save_meta().unwrap();

    let pm_session = Config::pm_tmux_session(&project);
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let engineer_session = Config::engineer_tmux_session(&project, &engineer.meta.name);
    let running_session = Config::researcher_tmux_session(&project, &running_agent.meta.name);
    let archived_session = Config::researcher_tmux_session(&project, &archived_agent.meta.name);
    let unrelated_session = format!("agman-unrelated-{unique}");
    let _cleanup = TmuxCleanup {
        sessions: vec![
            pm_session.clone(),
            task_session.clone(),
            engineer_session.clone(),
            running_session.clone(),
            archived_session.clone(),
            unrelated_session.clone(),
        ],
    };
    for session in [
        &pm_session,
        &task_session,
        &engineer_session,
        &running_session,
        &archived_session,
        &unrelated_session,
    ] {
        create_tmux_session(session, tmp.path());
    }

    use_cases::delete_project(&config, &project).unwrap();

    assert!(!Tmux::session_exists(&pm_session));
    assert!(!Tmux::session_exists(&task_session));
    assert!(!Tmux::session_exists(&engineer_session));
    assert!(!Tmux::session_exists(&running_session));
    assert!(!Tmux::session_exists(&archived_session));
    assert!(Tmux::session_exists(&unrelated_session));
}

#[test]
fn permanently_delete_archived_task_kills_leftover_task_session() {
    if !tmux_available() {
        eprintln!("skipping tmux lifecycle smoke: tmux binary is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let unique = unique_test_name();
    let project = format!("repo-{unique}");
    let branch = format!("branch-{unique}");
    let mut task = create_test_task(&config, &project, &branch);
    task.meta.archived_at = Some(chrono::Utc::now());
    task.save_meta().unwrap();
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let _cleanup = TmuxCleanup {
        sessions: vec![task_session.clone()],
    };
    create_tmux_session(&task_session, tmp.path());

    use_cases::permanently_delete_archived_task(&config, task).unwrap();

    assert!(!Tmux::session_exists(&task_session));
}

#[test]
fn ensure_task_tmux_backfills_attached_agent_windows_after_creation_and_recreation() {
    if !tmux_available() {
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

#[test]
fn ensure_task_tmux_disambiguates_compact_agent_window_label_collisions() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping tmux label collision smoke: tmux binary is unavailable");
        return;
    }

    let project = "proj";
    let first_name = "researcher-agent-window-names-9089";
    let second_name = "researcher-agent-window-names-10056";
    let first_session = Config::researcher_tmux_session(project, first_name);
    let second_session = Config::researcher_tmux_session(project, second_name);
    if Tmux::session_exists(&first_session) || Tmux::session_exists(&second_session) {
        eprintln!("skipping tmux label collision smoke: fixture session already exists");
        return;
    }

    let fixed_first = Tmux::linked_agent_window_name("researcher", first_name, &first_session);
    let fixed_second = Tmux::linked_agent_window_name("researcher", second_name, &second_session);
    assert_eq!(fixed_first, fixed_second);

    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    let branch = format!("branch-{}", unique_test_name());
    let task = create_test_task(&config, project, &branch);
    let task_id = task.meta.task_id();
    let first = create_test_researcher(&config, project, first_name);
    let second = create_test_researcher(&config, project, second_name);
    use_cases::attach_agent_to_task(&config, project, &first.meta.name, &task_id, None).unwrap();
    use_cases::attach_agent_to_task(&config, project, &second.meta.name, &task_id, None).unwrap();

    let engineer = use_cases::attached_engineer_for_task(&config, &task_id).unwrap();
    let engineer_session = Config::engineer_tmux_session(project, &engineer.meta.name);
    let task_session = task.meta.primary_repo().tmux_session.clone();
    let _cleanup = TmuxCleanup {
        sessions: vec![
            task_session.clone(),
            engineer_session.clone(),
            first_session.clone(),
            second_session.clone(),
        ],
    };

    create_tmux_session(&engineer_session, tmp.path());
    create_tmux_session(&first_session, tmp.path());
    create_tmux_session(&second_session, tmp.path());

    supervisor::ensure_task_tmux(&config, &task).unwrap();

    let windows = window_names(&task_session);
    let researcher_windows: Vec<_> = windows
        .iter()
        .filter(|name| name.starts_with("Researcher-"))
        .cloned()
        .collect();
    assert_eq!(researcher_windows.len(), 2, "{windows:?}");
    assert_ne!(researcher_windows[0], researcher_windows[1]);
    assert!(!researcher_windows.contains(&fixed_first));
    assert_compact_agent_windows(&windows);
    let first_canonical_windows = window_names(&first_session);
    let second_canonical_windows = window_names(&second_session);
    assert_eq!(first_canonical_windows.len(), 1);
    assert_eq!(second_canonical_windows.len(), 1);
    assert_ne!(first_canonical_windows[0], second_canonical_windows[0]);
    assert!(researcher_windows.contains(&first_canonical_windows[0]));
    assert!(researcher_windows.contains(&second_canonical_windows[0]));
}

fn unique_test_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{counter}-{nanos}", std::process::id())
}

fn tmux_available() -> bool {
    Command::new("tmux").arg("-V").output().is_ok()
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
