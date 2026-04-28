use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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
    /// - shell: runs git status
    /// - agman: shell where the supervisor launches an interactive claude
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

        // Create shell window
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "shell", "-c", wd])
            .output();
        Self::send_keys_to_window(
            session_name,
            "shell",
            "git status && git branch --show-current",
        )?;

        // Create agman window (just a shell; the supervisor launches an
        // interactive claude here when the task starts)
        let _ = Command::new("tmux")
            .args(["new-window", "-t", session_name, "-n", "agman", "-c", wd])
            .output();

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

        // Default to the "agman" window (where the supervisor runs claude),
        // not whichever window happened to be selected last. Best-effort:
        // sessions without an agman window (shouldn't happen for tasks) just
        // attach to their currently-selected window.
        let _ = Command::new("tmux")
            .args(["select-window", "-t", &format!("{}:agman", session_name)])
            .status();

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

    /// Ensure a tmux session exists with all standard windows.
    ///
    /// If the session already exists, this is a no-op. Otherwise, creates the
    /// session with `create_session_with_windows`.
    pub fn ensure_session(session_name: &str, working_dir: &Path) -> Result<()> {
        if Self::session_exists(session_name) {
            return Ok(());
        }
        tracing::info!(session = session_name, dir = %working_dir.display(), "recreating missing tmux session");
        Self::create_session_with_windows(session_name, working_dir)?;
        Ok(())
    }

    /// Create a simple agent tmux session with a single window running an interactive
    /// `claude` session. Used for Chief of Staff and PM agents (no nvim/lazygit/shell windows).
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
    ///
    /// Returns the spawned `Child` so callers can poll it with `try_wait`
    /// and keep the agman main loop ticking while the popup is open.
    pub fn popup_attach(session_name: &str) -> Result<std::process::Child> {
        tracing::info!(session = session_name, "opening popup attached to session");

        let attach_cmd = format!("tmux attach-session -t {}", session_name);
        Command::new("tmux")
            .args([
                "display-popup",
                "-E", // close popup when attach detaches
                "-w", "90%",
                "-h", "90%",
                &attach_cmd,
            ])
            .spawn()
            .context("failed to spawn tmux popup")
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

    /// Build a `claude` CLI command string that loads its system prompt from
    /// a file via `--system-prompt-file` and pins claude's session id to a
    /// caller-supplied UUID via `--session-id`.
    ///
    /// Pinning the session id (same pattern Chief of Staff/PM/researcher use on first
    /// launch) lets us store claude's actual session id in `session_history`,
    /// so the user can `claude --resume <id>` from the worktree to revisit a
    /// historical conversation. Without `--session-id`, claude would generate
    /// its own id internally and we'd have no easy way to learn it.
    ///
    /// Task agents never `--resume` — they are disposable, every flow step
    /// launches a fresh claude with a fresh id. This function therefore takes
    /// only a session id, not a resume id.
    ///
    /// File-based prompt delivery avoids embedding the prompt body in the
    /// shell command line (which can be megabytes once skills/footers are
    /// appended).
    pub fn build_claude_command_with_prompt_file(
        prompt_path: &Path,
        session_id: &str,
    ) -> String {
        let p = prompt_path.to_string_lossy().replace('\'', "'\\''");
        format!(
            "claude --dangerously-skip-permissions --system-prompt-file '{}' --session-id {}",
            p, session_id
        )
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

    /// Build a tmux target string for a session, optionally scoped to a window.
    /// When `window` is `None`, returns just the session name (targets the
    /// current window/pane). When `Some`, returns `session:window`.
    fn tmux_target(session_name: &str, window: Option<&str>) -> String {
        match window {
            Some(w) => format!("{}:{}", session_name, w),
            None => session_name.to_string(),
        }
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
        Self::inject_message_to(session_name, None, from, message, seq)
    }

    /// Like [`inject_message`] but scopes delivery to a specific window within
    /// the session (e.g. the `agman` window of a task session). When `window`
    /// is `None`, behaves exactly like `inject_message`.
    pub fn inject_message_to(
        session_name: &str,
        window: Option<&str>,
        from: &str,
        message: &str,
        seq: u64,
    ) -> Result<()> {
        let target = Self::tmux_target(session_name, window);
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
        tracing::trace!(session = session_name, target = %target, "pasting buffer with bracket paste mode");
        let paste_output = Command::new("tmux")
            .args(["paste-buffer", "-p", "-t", &target])
            .output()
            .context("failed to paste buffer into agent session")?;

        if !paste_output.status.success() {
            anyhow::bail!(
                "tmux paste-buffer failed: {}",
                String::from_utf8_lossy(&paste_output.stderr)
            );
        }

        // Give tmux time to process the pasted text before sending Enter
        std::thread::sleep(std::time::Duration::from_millis(300));

        tracing::trace!(session = session_name, target = %target, "sending Enter to submit message");
        let enter_output = Command::new("tmux")
            .args(["send-keys", "-t", &target, "Enter"])
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

        tracing::trace!(session = session_name, target = %target, "sending second Enter for reliability");
        let enter2_output = Command::new("tmux")
            .args(["send-keys", "-t", &target, "Enter"])
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
        Self::capture_pane_window(session_name, None)
    }

    /// Like [`capture_pane`] but scoped to a specific window within the session.
    pub fn capture_pane_window(session_name: &str, window: Option<&str>) -> Result<String> {
        let target = Self::tmux_target(session_name, window);
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-S", "-500", "-t", &target])
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
        Self::send_enter_to(session_name, None)
    }

    /// Like [`send_enter`] but scoped to a specific window within the session.
    pub fn send_enter_to(session_name: &str, window: Option<&str>) -> Result<()> {
        let target = Self::tmux_target(session_name, window);
        let output = Command::new("tmux")
            .args(["send-keys", "-t", &target, "Enter"])
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

    /// Check whether claude (or anything that isn't a shell) is the foreground
    /// process in the tmux session.
    ///
    /// We do not match on claude's own process name because Claude Code sets its
    /// `process.title` to the version string (e.g. `"2.1.107"`), which changes
    /// every release. Instead, we use the inverse: the pane was launched into a
    /// shell, and when claude runs it takes over the foreground. So "ready" ≡
    /// "foreground is not a known shell".
    pub fn is_claude_running(session_name: &str) -> Result<(bool, String)> {
        Self::is_claude_running_in(session_name, None)
    }

    /// Like [`is_claude_running`] but scoped to a specific window within the session.
    pub fn is_claude_running_in(
        session_name: &str,
        window: Option<&str>,
    ) -> Result<(bool, String)> {
        let target = Self::tmux_target(session_name, window);
        let output = Command::new("tmux")
            .args(["display-message", "-p", "-t", &target, "#{pane_current_command}"])
            .output()
            .context("failed to query pane_current_command")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to query pane_current_command: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh", "csh", "tcsh"];
        let is_shell = cmd.is_empty() || SHELLS.contains(&cmd.as_str());
        tracing::debug!(session = session_name, target = %target, cmd = %cmd, is_shell, "session readiness check");
        Ok((!is_shell, cmd))
    }

    /// Check whether a tmux session is ready to receive a message.
    ///
    /// Uses a shell denylist: returns true when the foreground process is NOT a
    /// known shell. Does not inspect pane content, so it cannot be poisoned by
    /// message text. Delivery reliability is ensured by the snippet-verification
    /// loop in the caller, not by UI scraping here.
    pub fn is_session_ready(session_name: &str) -> Result<(bool, String)> {
        Self::is_claude_running(session_name)
    }

    /// Like [`is_session_ready`] but scoped to a specific window.
    pub fn is_session_ready_in(
        session_name: &str,
        window: Option<&str>,
    ) -> Result<(bool, String)> {
        Self::is_claude_running_in(session_name, window)
    }

    /// Return the pane PID (`#{pane_pid}`) for the given session/window.
    ///
    /// Note: this is the pane's *shell* process (zsh/bash), not whatever
    /// foreground child it has spawned. The supervisor's kill path
    /// deliberately does **not** signal this pid — SIGTERM/SIGKILL on the
    /// shell tears down the entire tmux window. To stop a foreground
    /// `claude`, send `/exit` via `send_keys_to_window` instead. Kept for
    /// future diagnostic use.
    pub fn pane_pid(session_name: &str, window: Option<&str>) -> Result<Option<u32>> {
        let target = Self::tmux_target(session_name, window);
        let output = Command::new("tmux")
            .args(["display-message", "-p", "-t", &target, "#{pane_pid}"])
            .output()
            .context("failed to query pane_pid")?;

        if !output.status.success() {
            return Ok(None);
        }

        let pid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if pid_str.is_empty() {
            return Ok(None);
        }
        Ok(pid_str.parse::<u32>().ok())
    }
}
