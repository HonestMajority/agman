use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Clean initial content written to REVIEW.md
pub const REVIEW_MD_INITIAL: &str = "# Code Review\n\n(Review in progress...)\n";

pub fn pane_shows_claude_ready(content: &str) -> bool {
    let tail: Vec<&str> = content
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(40)
        .collect();

    let has_claude_chrome = tail.iter().any(|l| {
        l.contains("bypass permissions")
            || l.contains("-- INSERT --")
            || l.contains("Claude Code v")
    });

    let has_input_prompt = tail.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("> ") || t == ">" || t.starts_with('❯')
            || t.starts_with("│ >") || t.starts_with("│ ❯")
    });

    let modal_menu_active = tail.iter().any(|l| {
        l.contains("trust this folder")
            || l.contains("Is this a project you created")
            || l.contains("Enter to confirm · Esc to cancel")
    });

    has_claude_chrome && has_input_prompt && !modal_menu_active
}

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
                "-x",
                "200",
                "-y",
                "50",
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

    /// Ensure a tmux session exists with all standard windows (including review).
    ///
    /// If the session already exists, this is a no-op. Otherwise, creates the
    /// session with `create_session_with_windows` and adds the review window.
    pub fn ensure_session(session_name: &str, working_dir: &Path) -> Result<()> {
        if Self::session_exists(session_name) {
            return Ok(());
        }
        tracing::info!(session = session_name, dir = %working_dir.display(), "recreating missing tmux session");
        Self::create_session_with_windows(session_name, working_dir)?;
        Self::add_review_window(session_name, working_dir)?;
        Ok(())
    }

    /// Wipe REVIEW.md to a clean slate in the given working directory.
    pub fn wipe_review_md(working_dir: &Path) -> Result<()> {
        let review_md_path = working_dir.join("REVIEW.md");
        std::fs::write(&review_md_path, REVIEW_MD_INITIAL)
            .context("Failed to wipe REVIEW.md")?;
        Ok(())
    }

    /// Create a simple agent tmux session with a single window running an interactive
    /// `claude` session. Used for CEO and PM agents (no nvim/lazygit/shell windows).
    pub fn create_agent_session(
        session_name: &str,
        system_prompt: &str,
        resume_id: Option<&str>,
        session_id: Option<&str>,
        work_dir: Option<&Path>,
    ) -> Result<()> {
        if Self::session_exists(session_name) {
            tracing::debug!(session = session_name, "agent tmux session already exists");
            return Ok(());
        }

        tracing::info!(session = session_name, "creating agent tmux session");

        // Create the session with a default shell
        let mut args = vec!["new-session", "-d", "-x", "200", "-y", "50", "-s", session_name];
        let work_dir_str;
        if let Some(dir) = work_dir {
            work_dir_str = dir.to_string_lossy().to_string();
            args.extend(["-c", &work_dir_str]);
        }
        let output = Command::new("tmux")
            .args(&args)
            .output()
            .context("failed to create agent tmux session")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to create agent tmux session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Build and send the claude command
        let cmd = Self::build_claude_command(system_prompt, resume_id, session_id);
        Self::send_keys_to_session(session_name, &cmd)?;

        Ok(())
    }

    /// Open a tmux popup running an interactive `claude` session overlaid on
    /// the current pane. The popup closes when the claude session exits.
    pub fn display_popup(
        system_prompt: &str,
        resume_id: Option<&str>,
    ) -> Result<()> {
        let cmd = Self::build_claude_command(system_prompt, resume_id, None);

        tracing::info!("opening claude popup");

        let output = Command::new("tmux")
            .args([
                "display-popup",
                "-E",    // close popup when command exits
                "-w", "90%",
                "-h", "90%",
                &cmd,
            ])
            .output()
            .context("failed to open tmux popup")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to open tmux popup: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Open a tmux popup that attaches to an existing persistent session.
    /// When the user closes the popup (Esc), the session keeps running — the
    /// popup merely detaches from it.
    pub fn popup_attach(session_name: &str) -> Result<()> {
        tracing::info!(session = session_name, "opening popup attached to session");

        let attach_cmd = format!("tmux attach-session -t {}", session_name);
        let output = Command::new("tmux")
            .args([
                "display-popup",
                "-E", // close popup when attach detaches
                "-w", "90%",
                "-h", "90%",
                &attach_cmd,
            ])
            .output()
            .context("failed to open tmux popup")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to open tmux popup: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Build a `claude` CLI command string with system prompt and optional resume.
    ///
    /// When `resume_id` is `Some`, emits `--resume <id>` WITHOUT `--system-prompt`
    /// (the resumed session already has all context). When `resume_id` is `None`,
    /// emits `--system-prompt` as before, and optionally `--session-id` to pin the
    /// session for future resumption.
    fn build_claude_command(
        system_prompt: &str,
        resume_id: Option<&str>,
        session_id: Option<&str>,
    ) -> String {
        let mut cmd = String::from("claude --dangerously-skip-permissions");
        if let Some(id) = resume_id {
            cmd.push_str(&format!(" --resume {}", id));
        } else {
            // Shell-escape the prompt by using single quotes with inner escaping
            let escaped_prompt = system_prompt.replace('\'', "'\\''");
            cmd.push_str(&format!(" --system-prompt '{}'", escaped_prompt));
            if let Some(id) = session_id {
                cmd.push_str(&format!(" --session-id {}", id));
            }
        }
        cmd
    }

    /// Send keys to the first (and only) window/pane of a session.
    pub fn send_keys_to_session(session_name: &str, keys: &str) -> Result<()> {
        tracing::trace!(session = session_name, "sending keys to agent session");
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session_name, keys, "C-m"])
            .output()
            .context("failed to send keys to agent session")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to send keys to agent session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Inject a formatted message into an agent's tmux session.
    ///
    /// Uses `tmux load-buffer -` (stdin pipe) + `paste-buffer -p` (bracket paste
    /// mode) to paste the message as a single block. This preserves newlines in
    /// multiline messages — bracket paste wraps the content in escape sequences
    /// that prevent the terminal from interpreting newlines as Enter presses.
    /// A separate Enter key event is then sent to submit the message.
    ///
    /// The `seq` parameter is the unique message sequence number, used to build
    /// a delivery tag (`[msg:{from}:{seq}]`) so each message can be uniquely
    /// identified in the scrollback for verification.
    pub fn inject_message(session_name: &str, from: &str, message: &str, seq: u64) -> Result<()> {
        let formatted = format!("[msg:{}:{}] [Message from {}]: {}", from, seq, from, message);

        // Load message into tmux paste buffer via stdin (avoids shell escaping issues)
        tracing::trace!(session = session_name, "loading message into tmux buffer");
        let mut load_child = Command::new("tmux")
            .args(["load-buffer", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn tmux load-buffer")?;

        load_child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(formatted.as_bytes())
            .context("failed to write message to tmux load-buffer stdin")?;

        let load_output = load_child
            .wait_with_output()
            .context("failed to wait for tmux load-buffer")?;

        if !load_output.status.success() {
            anyhow::bail!(
                "tmux load-buffer failed: {}",
                String::from_utf8_lossy(&load_output.stderr)
            );
        }

        // Paste buffer into target session with bracket paste mode
        tracing::trace!(session = session_name, "pasting buffer with bracket paste mode");
        let paste_output = Command::new("tmux")
            .args(["paste-buffer", "-p", "-t", session_name])
            .output()
            .context("failed to paste buffer into agent session")?;

        if !paste_output.status.success() {
            anyhow::bail!(
                "tmux paste-buffer failed: {}",
                String::from_utf8_lossy(&paste_output.stderr)
            );
        }

        // Give tmux time to process the pasted text before sending Enter
        std::thread::sleep(std::time::Duration::from_millis(150));

        tracing::trace!(session = session_name, "sending Enter to submit message");
        let enter_output = Command::new("tmux")
            .args(["send-keys", "-t", session_name, "Enter"])
            .output()
            .context("failed to send Enter to agent session")?;

        if !enter_output.status.success() {
            anyhow::bail!(
                "failed to send Enter to agent session: {}",
                String::from_utf8_lossy(&enter_output.stderr)
            );
        }

        // Send a second Enter as a safety measure — some terminal states absorb the first
        std::thread::sleep(std::time::Duration::from_millis(80));

        tracing::trace!(session = session_name, "sending second Enter for reliability");
        let enter2_output = Command::new("tmux")
            .args(["send-keys", "-t", session_name, "Enter"])
            .output()
            .context("failed to send second Enter to agent session")?;

        if !enter2_output.status.success() {
            anyhow::bail!(
                "failed to send second Enter to agent session: {}",
                String::from_utf8_lossy(&enter2_output.stderr)
            );
        }

        Ok(())
    }

    /// Capture the content of a tmux pane including scrollback history for delivery verification.
    pub fn capture_pane(session_name: &str) -> Result<String> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-S", "-500", "-t", session_name])
            .output()
            .context("failed to capture tmux pane")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to capture tmux pane: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Send just an Enter key press to a session (for retry when text was pasted
    /// but Enter didn't register).
    pub fn send_enter(session_name: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session_name, "Enter"])
            .output()
            .context("failed to send Enter to agent session")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to send Enter to agent session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Non-blocking check whether a tmux session has Claude Code ready for input.
    ///
    /// Issues a SIGWINCH nudge (resize-window) to force a re-render, then checks
    /// the last 40 non-empty lines for Claude chrome, input prompt, and modal menus.
    pub fn is_session_ready(session_name: &str) -> Result<bool> {
        let _ = Command::new("tmux")
            .args(["resize-window", "-t", session_name, "-x", "200", "-y", "50"])
            .output();

        let content = Self::capture_pane(session_name)?;

        let tail: Vec<&str> = content
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(40)
            .collect();

        let has_claude_chrome = tail.iter().any(|l| {
            l.contains("bypass permissions")
                || l.contains("-- INSERT --")
                || l.contains("Claude Code v")
        });
        let has_input_prompt = tail.iter().any(|l| {
            let t = l.trim_start();
            t.starts_with("> ") || t == ">" || t.starts_with('❯')
                || t.starts_with("│ >") || t.starts_with("│ ❯")
        });
        let modal_menu_active = tail.iter().any(|l| {
            l.contains("trust this folder")
                || l.contains("Is this a project you created")
                || l.contains("Enter to confirm · Esc to cancel")
        });

        let ready = has_claude_chrome && has_input_prompt && !modal_menu_active;

        tracing::debug!(
            session = session_name,
            has_claude_chrome,
            has_input_prompt,
            modal_menu_active,
            ready,
            "session readiness check",
        );
        Ok(ready)
    }
}
