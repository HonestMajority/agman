use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct Tmux;

impl Tmux {
    pub fn session_exists(session_name: &str) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn create_session(session_name: &str, working_dir: &Path) -> Result<()> {
        if Self::session_exists(session_name) {
            return Ok(());
        }

        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                session_name,
                "-c",
                working_dir.to_str().unwrap(),
            ])
            .output()
            .context("Failed to create tmux session")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to create tmux session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    pub fn kill_session(session_name: &str) -> Result<()> {
        if !Self::session_exists(session_name) {
            return Ok(());
        }

        let output = Command::new("tmux")
            .args(["kill-session", "-t", session_name])
            .output()
            .context("Failed to kill tmux session")?;

        if !output.status.success() {
            tracing::warn!(
                "Failed to kill tmux session {}: {}",
                session_name,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    pub fn attach_session(session_name: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args(["attach-session", "-t", session_name])
            .status()
            .context("Failed to attach to tmux session")?;

        if !status.success() {
            anyhow::bail!("Failed to attach to tmux session");
        }

        Ok(())
    }

    pub fn send_keys(session_name: &str, keys: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session_name, keys, "Enter"])
            .output()
            .context("Failed to send keys to tmux session")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to send keys: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn capture_pane(session_name: &str, lines: Option<usize>) -> Result<String> {
        let mut args = vec!["capture-pane", "-t", session_name, "-p"];

        let lines_str;
        if let Some(n) = lines {
            lines_str = format!("-{}", n);
            args.push("-S");
            args.push(&lines_str);
        }

        let output = Command::new("tmux")
            .args(&args)
            .output()
            .context("Failed to capture tmux pane")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to capture pane: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    #[allow(dead_code)]
    pub fn list_sessions() -> Result<Vec<String>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .context("Failed to list tmux sessions")?;

        if !output.status.success() {
            // No sessions is not an error
            return Ok(Vec::new());
        }

        let sessions = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect();

        Ok(sessions)
    }

    #[allow(dead_code)]
    pub fn is_session_running(session_name: &str) -> bool {
        // Check if there's an active process in the session
        let output = Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                session_name,
                "-F",
                "#{pane_current_command}",
            ])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let cmd = String::from_utf8_lossy(&o.stdout);
                let cmd = cmd.trim();
                // If it's just a shell, nothing is running
                !matches!(cmd, "bash" | "zsh" | "sh" | "fish")
            }
            _ => false,
        }
    }
}
