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
        build_linked_agent_window_name(kind, agent_name, canonical_session)
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
        if Self::window_exists(task_session, window_name)? {
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

    pub fn unlink_agent_window(task_session: &str, window_name: &str) -> Result<()> {
        if !Self::session_exists(task_session) || !Self::window_exists(task_session, window_name)? {
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
    _window_name: &str,
) -> Vec<String> {
    vec![
        "link-window".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        canonical_session.to_string(),
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

fn build_linked_agent_window_name(kind: &str, agent_name: &str, canonical_session: &str) -> String {
    let suffix = format!("{:08x}", stable_hash32(canonical_session));
    let prefix = sanitize_tmux_window_component(kind);
    let mut name = sanitize_tmux_window_component(agent_name);
    let max_name = 80usize.saturating_sub(prefix.len() + suffix.len() + 2);
    if name.len() > max_name {
        name.truncate(max_name);
        name = name.trim_end_matches('-').to_string();
    }
    format!("{prefix}-{name}-{suffix}")
}

fn stable_hash32(raw: &str) -> u32 {
    let mut hash = 0x811c9dc5_u32;
    for byte in raw.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

fn sanitize_tmux_window_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if !last_dash {
                out.push(mapped);
            }
            last_dash = true;
        } else {
            out.push(mapped);
            last_dash = false;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "agent".to_string()
    } else {
        trimmed.to_string()
    }
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
                "agent-session",
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

        assert!(one.starts_with("engineer-fix-pr-review-"));
        assert!(one.len() <= 80);
        assert_ne!(one, two);
        assert!(one
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
    }
}
