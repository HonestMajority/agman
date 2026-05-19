use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const SHELL_COMMANDS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh", "csh", "tcsh"];

pub struct Tmux;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWindowActivity {
    pub session_name: String,
    pub window_activity: Option<i64>,
    pub pane_current_command: String,
    pub pane_dead: bool,
}

impl Tmux {
    pub fn is_shell_command(cmd: &str) -> bool {
        let cmd = cmd.trim();
        cmd.is_empty() || SHELL_COMMANDS.contains(&cmd)
    }

    pub fn session_exists(session_name: &str) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Create a new task tmux session with multiple worktree windows:
    /// - nvim: starts nvim
    /// - lazygit: starts lazygit
    /// - shell: runs git status
    ///
    /// Attached agents are linked in from their canonical sessions separately;
    /// task sessions no longer own a manual `agman` window.
    pub fn create_session_with_windows(session_name: &str, working_dir: &Path) -> Result<()> {
        if Self::session_exists(session_name) {
            tracing::debug!(
                session = session_name,
                "tmux session already exists, skipping creation"
            );
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

        // Default to the editor window for task sessions. Agent interaction
        // happens in canonical agent sessions or linked task-session windows.
        let _ = Command::new("tmux")
            .args(["select-window", "-t", &format!("{}:nvim", session_name)])
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
        tracing::trace!(
            session = session_name,
            window = window_name,
            "sending keys to tmux window"
        );
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

    /// Send literal text to a specific window without pressing Enter.
    pub fn send_text_to_window(session_name: &str, window_name: &str, text: &str) -> Result<()> {
        Self::send_text_to_target(&Self::tmux_target(session_name, Some(window_name)), text)
    }

    /// Send literal text to a session's current pane without pressing Enter.
    pub fn send_text_to_session(session_name: &str, text: &str) -> Result<()> {
        Self::send_text_to_target(session_name, text)
    }

    fn send_text_to_target(target: &str, text: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", target, text])
            .output()
            .context("failed to send text to tmux target")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to send text to tmux target: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Send Ctrl+C to a specific window to interrupt any running process
    pub fn send_ctrl_c_to_window(session_name: &str, window_name: &str) -> Result<()> {
        Self::send_ctrl_c_target(&Self::tmux_target(session_name, Some(window_name)))
    }

    /// Send Ctrl+C to a session's currently-focused pane (no window scope).
    pub fn send_ctrl_c_to_session(session_name: &str) -> Result<()> {
        Self::send_ctrl_c_target(session_name)
    }

    fn send_ctrl_c_target(target: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", target, "C-c"])
            .output()
            .context("Failed to send Ctrl+C to tmux target")?;

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

    pub fn linked_agent_window_name(
        kind: &str,
        agent_name: &str,
        canonical_session: &str,
    ) -> String {
        let suffix = linked_agent_window_hash_suffix(canonical_session, 4);
        build_linked_agent_window_name(kind, agent_name, &suffix)
    }

    pub(crate) fn linked_agent_window_name_with_suffix(
        kind: &str,
        agent_name: &str,
        suffix: &str,
    ) -> String {
        build_linked_agent_window_name(kind, agent_name, suffix)
    }

    pub(crate) fn linked_agent_window_hash_suffix(canonical_session: &str, len: usize) -> String {
        linked_agent_window_hash_suffix(canonical_session, len)
    }

    pub fn link_agent_window(
        task_session: &str,
        canonical_session: &str,
        window_name: &str,
    ) -> Result<()> {
        if !Self::session_exists(task_session) {
            anyhow::bail!("task tmux session '{task_session}' does not exist");
        }
        if !Self::session_exists(canonical_session) {
            anyhow::bail!("agent tmux session '{canonical_session}' does not exist");
        }
        if Self::window_exists(task_session, window_name)?
            && Self::same_window_id(canonical_session, &format!("{task_session}:{window_name}"))?
        {
            return Ok(());
        }

        let rename_args = rename_window_args(canonical_session, window_name);
        let rename_output = Command::new("tmux")
            .args(rename_args.iter().map(String::as_str))
            .output()
            .context("failed to rename canonical agent tmux window")?;
        if !rename_output.status.success() {
            anyhow::bail!(
                "failed to rename canonical agent tmux window: {}",
                String::from_utf8_lossy(&rename_output.stderr)
            );
        }
        if Self::window_exists(task_session, window_name)? {
            if Self::same_window_id(canonical_session, &format!("{task_session}:{window_name}"))? {
                return Ok(());
            }
            anyhow::bail!(
                "task tmux session '{task_session}' already has a different window named '{window_name}'"
            );
        }

        let args = link_window_args(task_session, canonical_session, window_name);
        let output = Command::new("tmux")
            .args(args.iter().map(String::as_str))
            .output()
            .context("failed to link agent window into task session")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to link agent window into task session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    pub fn unlink_agent_window(
        task_session: &str,
        canonical_session: &str,
        window_name: &str,
    ) -> Result<()> {
        if !Self::session_exists(task_session) || !Self::window_exists(task_session, window_name)? {
            return Ok(());
        }
        if Self::session_exists(canonical_session)
            && !Self::same_window_id(canonical_session, &format!("{task_session}:{window_name}"))?
        {
            return Ok(());
        }

        let args = unlink_window_args(task_session, window_name);
        let output = Command::new("tmux")
            .args(args.iter().map(String::as_str))
            .output()
            .context("failed to unlink agent window from task session")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to unlink agent window from task session: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    fn same_window_id(left_target: &str, right_target: &str) -> Result<bool> {
        let Some(left_id) = Self::window_id(left_target)? else {
            return Ok(false);
        };
        let Some(right_id) = Self::window_id(right_target)? else {
            return Ok(false);
        };
        Ok(left_id == right_id)
    }

    fn window_id(target: &str) -> Result<Option<String>> {
        let output = Command::new("tmux")
            .args(["display-message", "-p", "-t", target, "#{window_id}"])
            .output()
            .context("failed to query tmux window id")?;
        if !output.status.success() {
            return Ok(None);
        }
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if id.is_empty() {
            Ok(None)
        } else {
            Ok(Some(id))
        }
    }

    fn window_exists(session_name: &str, window_name: &str) -> Result<bool> {
        let output = Command::new("tmux")
            .args(["list-windows", "-t", session_name, "-F", "#{window_name}"])
            .output()
            .context("failed to list tmux windows")?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to list tmux windows: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == window_name))
    }

    /// Create a simple agent tmux session with a single window running an
    /// interactive harness. Used for Chief of Staff/PM/researcher
    /// and task agents.
    ///
    /// `command` is the harness-built shell command to launch (see
    /// `Harness::build_session_command`). The supervisor mints it via the
    /// configured `Box<dyn Harness>` and passes it through.
    pub fn create_agent_session(
        session_name: &str,
        command: &str,
        work_dir: Option<&Path>,
    ) -> Result<()> {
        if Self::session_exists(session_name) {
            tracing::debug!(session = session_name, "agent tmux session already exists");
            return Ok(());
        }

        tracing::info!(session = session_name, "creating agent tmux session");

        // Create the session with a default shell
        let mut args = vec![
            "new-session",
            "-d",
            "-x",
            "200",
            "-y",
            "50",
            "-s",
            session_name,
        ];
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

        Self::send_keys_to_session(session_name, command)?;

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
                "-w",
                "90%",
                "-h",
                "90%",
                &attach_cmd,
            ])
            .spawn()
            .context("failed to spawn tmux popup")
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
        let formatted = format!(
            "[msg:{}:{}] [Message from {}]: {}",
            from, seq, from, message
        );

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

    /// Query activity for every tmux window in one call.
    pub fn list_window_activity() -> Result<Vec<TmuxWindowActivity>> {
        let output = Command::new("tmux")
            .args([
                "list-windows",
                "-a",
                "-F",
                "#{session_name}\t#{window_activity}\t#{pane_current_command}\t#{pane_dead}",
            ])
            .output()
            .context("failed to query tmux window activity")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to query tmux window activity: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut windows = Vec::new();
        for line in stdout.lines() {
            let mut parts = line.splitn(4, '\t');
            let session_name = parts.next().unwrap_or_default().to_string();
            if session_name.is_empty() {
                continue;
            }
            let window_activity = parts.next().and_then(|s| {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    trimmed.parse::<i64>().ok()
                }
            });
            let pane_current_command = parts.next().unwrap_or_default().trim().to_string();
            let pane_dead = matches!(parts.next().unwrap_or_default().trim(), "1" | "true");
            windows.push(TmuxWindowActivity {
                session_name,
                window_activity,
                pane_current_command,
                pane_dead,
            });
        }

        Ok(windows)
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

    /// Check whether the configured agent harness (or anything that isn't a
    /// shell) is the foreground process in the tmux session.
    ///
    /// We do not match on the harness's own process name because harness CLIs
    /// can set `process.title` to a version string, which
    /// changes every release. Instead, we use the inverse: the pane was
    /// launched into a shell, and when the agent runs it takes over the
    /// foreground. So "ready" ≡ "foreground is not a known shell".
    pub fn is_agent_running(session_name: &str) -> Result<(bool, String)> {
        Self::is_agent_running_in(session_name, None)
    }

    /// Like [`is_agent_running`] but scoped to a specific window within the session.
    pub fn is_agent_running_in(session_name: &str, window: Option<&str>) -> Result<(bool, String)> {
        let target = Self::tmux_target(session_name, window);
        let output = Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                &target,
                "#{pane_current_command}",
            ])
            .output()
            .context("failed to query pane_current_command")?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to query pane_current_command: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let is_shell = Self::is_shell_command(&cmd);
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
        Self::is_agent_running(session_name)
    }

    /// Like [`is_session_ready`] but scoped to a specific window.
    pub fn is_session_ready_in(session_name: &str, window: Option<&str>) -> Result<(bool, String)> {
        Self::is_agent_running_in(session_name, window)
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

pub fn link_window_args(
    task_session: &str,
    canonical_session: &str,
    window_name: &str,
) -> Vec<String> {
    vec![
        "link-window".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        format!("{canonical_session}:{window_name}"),
        "-t".to_string(),
        format!("{task_session}:"),
    ]
}

pub fn rename_window_args(canonical_session: &str, window_name: &str) -> Vec<String> {
    vec![
        "rename-window".to_string(),
        "-t".to_string(),
        canonical_session.to_string(),
        window_name.to_string(),
    ]
}

pub fn unlink_window_args(task_session: &str, window_name: &str) -> Vec<String> {
    vec![
        "unlink-window".to_string(),
        "-t".to_string(),
        format!("{task_session}:{window_name}"),
    ]
}

fn build_linked_agent_window_name(kind: &str, agent_name: &str, suffix: &str) -> String {
    const MAX_WINDOW_NAME_LEN: usize = 24;

    let prefix = title_case_kind(kind);
    let max_slug_len = MAX_WINDOW_NAME_LEN.saturating_sub(prefix.len() + suffix.len() + 2);
    let slug = compact_agent_slug(kind, agent_name, max_slug_len);
    format!("{prefix}-{slug}-{suffix}")
}

fn stable_hash32(raw: &str) -> u32 {
    let mut hash = 0x811c9dc5_u32;
    for byte in raw.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

fn linked_agent_window_hash_suffix(raw: &str, len: usize) -> String {
    let mut hash = stable_hash32(raw) as u64;
    let base = 36_u64.pow(len as u32);
    hash %= base;

    let mut chars = vec!['0'; len];
    for idx in (0..len).rev() {
        let digit = (hash % 36) as u8;
        chars[idx] = match digit {
            0..=9 => (b'0' + digit) as char,
            _ => (b'a' + digit - 10) as char,
        };
        hash /= 36;
    }
    chars.into_iter().collect()
}

fn title_case_kind(raw: &str) -> String {
    match sanitize_tmux_window_tokens(raw).first().map(String::as_str) {
        Some("engineer") => "Engineer".to_string(),
        Some("researcher") => "Researcher".to_string(),
        Some("reviewer") => "Reviewer".to_string(),
        Some("tester") => "Tester".to_string(),
        Some("operator") => "Operator".to_string(),
        Some(other) => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => {
                    format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
                }
                None => "Agent".to_string(),
            }
        }
        None => "Agent".to_string(),
    }
}

fn compact_agent_slug(kind: &str, agent_name: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let kind_token = sanitize_tmux_window_tokens(kind)
        .into_iter()
        .next()
        .unwrap_or_default();
    let mut tokens = sanitize_tmux_window_tokens(agent_name);
    if tokens
        .first()
        .is_some_and(|token| !kind_token.is_empty() && token == &kind_token)
    {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        tokens.push("agent".to_string());
    }

    let full = tokens.join("-");
    if full.len() <= max_len {
        return full;
    }

    if tokens.len() > 1 {
        let first = truncate_ascii_token(&tokens[0], max_len);
        if first.len() + 2 <= max_len {
            let rest_len = max_len - first.len() - 1;
            let second = truncate_ascii_token(&tokens[1], rest_len);
            if !second.is_empty() {
                return format!("{first}-{second}");
            }
        }
    }

    truncate_ascii_token(&full, max_len)
        .trim_matches('-')
        .to_string()
}

fn truncate_ascii_token(raw: &str, max_len: usize) -> String {
    raw.chars().take(max_len).collect()
}

fn sanitize_tmux_window_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::{link_window_args, rename_window_args, unlink_window_args, Tmux};

    #[test]
    fn link_window_command_targets_canonical_agent_window() {
        assert_eq!(
            link_window_args("task-session", "agent-session", "engineer-main-12345678"),
            vec![
                "link-window",
                "-d",
                "-s",
                "agent-session:engineer-main-12345678",
                "-t",
                "task-session:",
            ]
        );
    }

    #[test]
    fn rename_window_command_names_canonical_agent_window() {
        assert_eq!(
            rename_window_args("agent-session", "engineer-main-12345678"),
            vec![
                "rename-window",
                "-t",
                "agent-session",
                "engineer-main-12345678",
            ]
        );
    }

    #[test]
    fn unlink_window_command_targets_linked_task_window() {
        assert_eq!(
            unlink_window_args("task-session", "reviewer-pr-12345678"),
            vec!["unlink-window", "-t", "task-session:reviewer-pr-12345678",]
        );
    }

    #[test]
    fn linked_agent_window_names_are_sanitized_and_collision_safe() {
        let one = Tmux::linked_agent_window_name("Engineer", "Fix/PR:Review", "agent-session-a");
        let two = Tmux::linked_agent_window_name("Engineer", "Fix/PR:Review", "agent-session-b");

        assert!(one.starts_with("Engineer-fix-pr-"));
        assert!(one.len() <= 24);
        assert_ne!(one, two);
        assert!(one
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
    }

    #[test]
    fn linked_agent_window_names_strip_redundant_kind_and_stay_short() {
        let name = Tmux::linked_agent_window_name(
            "engineer",
            "engineer-agman-remove-redundant-poison-error-closure",
            "agman-engineer-agman-improvements-engineer-agman-remove-redundant-poison-error-closure",
        );

        assert!(name.starts_with("Engineer-agman-"));
        assert!(!name.starts_with("Engineer-engineer-"));
        assert!(name.len() <= 24, "{name}");
    }

    #[test]
    fn linked_agent_window_names_title_case_known_kinds() {
        assert!(
            Tmux::linked_agent_window_name("researcher", "agent-window-names", "s1")
                .starts_with("Researcher-")
        );
        assert!(
            Tmux::linked_agent_window_name("reviewer", "pr5590-ltv", "s2").starts_with("Reviewer-")
        );
        assert!(Tmux::linked_agent_window_name("tester", "smoke", "s3").starts_with("Tester-"));
        assert!(Tmux::linked_agent_window_name("operator", "deploy", "s4").starts_with("Operator-"));
    }

    #[test]
    fn linked_agent_window_suffix_is_deterministic() {
        let one = Tmux::linked_agent_window_name("Engineer", "agent-window-names", "canonical-a");
        let two = Tmux::linked_agent_window_name("Engineer", "agent-window-names", "canonical-a");
        assert_eq!(one, two);

        let suffix = one.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 4);
        assert!(suffix.chars().all(|ch| ch.is_ascii_alphanumeric()));
    }

    #[test]
    fn linked_agent_window_names_keep_same_slug_sessions_distinct() {
        let one = Tmux::linked_agent_window_name("Reviewer", "pr5590-ltv", "canonical-a");
        let two = Tmux::linked_agent_window_name("Reviewer", "pr5590-ltv", "canonical-b");

        assert_ne!(one, two);
        assert_eq!(
            one.rsplit_once('-').unwrap().0,
            two.rsplit_once('-').unwrap().0
        );
    }

    #[test]
    fn linked_agent_window_names_handle_empty_sanitized_names() {
        let name = Tmux::linked_agent_window_name("Engineer", "!!!", "canonical");

        assert!(name.starts_with("Engineer-agent-"));
        assert!(name.len() <= 24);
    }
}
