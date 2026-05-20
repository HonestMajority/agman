use agman::tmux::Tmux;
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
fn detached_tmux_window_is_not_visible_to_any_client() {
    if !tmux_available() {
        eprintln!("skipping tmux visibility smoke: tmux binary is unavailable");
        return;
    }

    let session = format!("agman-visibility-{}", unique_test_name());
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = TmuxCleanup {
        sessions: vec![session.clone()],
    };
    create_tmux_session(&session, tmp.path());

    assert!(!Tmux::is_window_visible_to_any_client(&session, None).unwrap());
}

#[test]
fn missing_tmux_target_returns_visibility_error() {
    if !tmux_available() {
        eprintln!("skipping tmux visibility smoke: tmux binary is unavailable");
        return;
    }

    assert!(
        Tmux::is_window_visible_to_any_client("agman-visibility-missing-session", None).is_err()
    );
}

fn unique_test_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
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
