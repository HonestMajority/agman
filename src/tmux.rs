use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Clean initial content written to REVIEW.md
pub const REVIEW_MD_INITIAL: &str = "# Code Review\n\n(Review in progress...)\n";

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
    /// - shell: runs git status
    /// - agman: shell for agent commands
    pub fn create_session_with_windows(session_name: &str, working_dir: &Path) -> Result<()> {
        if Self::session_exists(session_name) {
            tracing::debug!(session = session_name, "tmux session already exists, skipping creation");
            return Ok(());
        }
        tracing::debug!(session = session_name, dir = %working_dir.display(), "creating tmux session");

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

        // Create shell window
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "shell", "-c", wd])
            .output();
        Self::send_keys_to_window(
            session_name,
            "shell",
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
        tracing::debug!(session = session_name, "killing tmux session");

        let output = Command::new("tmux")
            .args(["kill-session", "-t", session_name])
            .output()
            .context("Failed to kill tmux session")?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(session = session_name, error = %err, "failed to kill tmux session");
        }

        Ok(())
    }

    pub fn attach_session(session_name: &str) -> Result<()> {
        tracing::debug!(session = session_name, "attaching to tmux session");
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
        tracing::trace!(session = session_name, window = window_name, "sending keys to tmux window");
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

    /// Add a "review" tmux window with nvim REVIEW.md, then swap it before "agman"
    /// so that review is window 5 and agman is window 6.
    ///
    /// Pre-creates REVIEW.md in working_dir if it doesn't already exist.
    pub fn add_review_window(session_name: &str, working_dir: &Path) -> Result<()> {
        // Pre-create REVIEW.md so nvim can open it immediately
        let review_md_path = working_dir.join("REVIEW.md");
        if !review_md_path.exists() {
            std::fs::write(&review_md_path, REVIEW_MD_INITIAL)?;
        }

        let wd = working_dir.to_str().unwrap_or(".");

        // Create the review window (appended after agman, so it becomes window 6)
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "review", "-c", wd])
            .output();
        Self::send_keys_to_window(session_name, "review", "nvim REVIEW.md")?;

        // Swap review (window 6) and agman (window 5) so review=5, agman=6
        let review_target = format!("{}:review", session_name);
        let agman_target = format!("{}:agman", session_name);
        let _ = Command::new("tmux")
            .args(["swap-window", "-s", &review_target, "-t", &agman_target])
            .output();

        Ok(())
    }

    /// Wipe REVIEW.md to a clean slate in the given working directory.
    pub fn wipe_review_md(working_dir: &Path) -> Result<()> {
        let review_md_path = working_dir.join("REVIEW.md");
        std::fs::write(&review_md_path, REVIEW_MD_INITIAL)
            .context("Failed to wipe REVIEW.md")?;
        Ok(())
    }
}
