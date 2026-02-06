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

    /// Create a new tmux session with multiple windows:
    /// - nvim: starts nvim
    /// - lazygit: starts lazygit
    /// - claude: starts claude --dangerously-skip-permissions
    /// - zsh: runs git status
    /// - agman: shell for agent commands
    pub fn create_session_with_windows(session_name: &str, working_dir: &Path) -> Result<()> {
        if Self::session_exists(session_name) {
            return Ok(());
        }

        let wd = working_dir.to_str().unwrap();

        // Create session with first window (nvim)
        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                session_name,
                "-c",
                wd,
                "-n",
                "nvim",
            ])
            .output()
            .context("Failed to create tmux session")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to create tmux session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Start nvim in first window
        Self::send_keys_to_window(session_name, "nvim", "nvim")?;

        // Create lazygit window
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "lazygit", "-c", wd])
            .output();
        Self::send_keys_to_window(session_name, "lazygit", "lazygit")?;

        // Create claude window
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "claude", "-c", wd])
            .output();
        Self::send_keys_to_window(
            session_name,
            "claude",
            "claude --dangerously-skip-permissions",
        )?;

        // Create zsh window
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "zsh", "-c", wd])
            .output();
        Self::send_keys_to_window(
            session_name,
            "zsh",
            "git status && git branch --show-current",
        )?;

        // Create agman window (just a shell, agents will send commands here)
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "agman", "-c", wd])
            .output();
        // Don't start agman interactively - agents will send commands to this window

        // Select nvim window as default
        let _ = Command::new("tmux")
            .args(["select-window", "-t", &format!("{}:nvim", session_name)])
            .output();

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
        // Try switch-client first (if already in tmux)
        let switch_result = Command::new("tmux")
            .args(["switch-client", "-t", session_name])
            .status();

        if let Ok(status) = switch_result {
            if status.success() {
                return Ok(());
            }
        }

        // Fall back to attach-session (if not in tmux)
        let status = Command::new("tmux")
            .args(["attach-session", "-t", session_name])
            .status()
            .context("Failed to attach to tmux session")?;

        if !status.success() {
            anyhow::bail!("Failed to attach to tmux session");
        }

        Ok(())
    }

    /// Send keys to a specific window in a session
    pub fn send_keys_to_window(session_name: &str, window_name: &str, keys: &str) -> Result<()> {
        let target = format!("{}:{}", session_name, window_name);
        let output = Command::new("tmux")
            .args(["send-keys", "-t", &target, keys, "C-m"])
            .output()
            .context("Failed to send keys to tmux window")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to send keys: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Send Ctrl+C to a specific window to interrupt any running process
    pub fn send_ctrl_c_to_window(session_name: &str, window_name: &str) -> Result<()> {
        let target = format!("{}:{}", session_name, window_name);
        let output = Command::new("tmux")
            .args(["send-keys", "-t", &target, "C-c"])
            .output()
            .context("Failed to send Ctrl+C to tmux window")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to send Ctrl+C: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Send keys to the default pane in a session
    #[allow(dead_code)]
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
