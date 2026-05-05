//! Pi harness implementation.

use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::{Harness, HarnessKind, LaunchContext, RegisterContext, SessionKey};

const TOOL_ALLOWLIST: &str = "read,bash,edit,write,grep,find,ls";

pub struct PiHarness;

impl Harness for PiHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Pi
    }

    fn cli_binary(&self) -> &'static str {
        "pi"
    }

    fn install_hint(&self) -> &'static str {
        "npm install -g @mariozechner/pi-coding-agent"
    }

    fn skill_hint(&self) -> &'static str {
        ""
    }

    fn build_session_command(&self, ctx: &LaunchContext) -> String {
        let identity_file = ctx
            .identity_file
            .expect("pi launches require LaunchContext::identity_file");
        let session_dir = ctx
            .session_dir
            .expect("pi launches require LaunchContext::session_dir");

        let identity_file = super::shell_single_quote(&identity_file.to_string_lossy());
        let session_dir = super::shell_single_quote(&session_dir.to_string_lossy());

        let mut cmd = format!(
            "PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 pi --offline --append-system-prompt {} --session-dir {} --tools {}",
            identity_file, session_dir, TOOL_ALLOWLIST
        );
        if matches!(ctx.session_key, SessionKey::Resume(_)) {
            cmd.push_str(" --continue");
        }
        cmd
    }

    fn ensure_workspace_trusted(&self, _cwd: &Path) -> Result<()> {
        Ok(())
    }

    fn register_session_name(&self, ctx: &RegisterContext) -> Result<()> {
        let target = match ctx.window {
            Some(w) => format!("{}:{}", ctx.session, w),
            None => ctx.session.to_string(),
        };
        let cmd = format!("/name {}", ctx.name);

        let sent = register_session_name_with_retry(
            || paste_text(&target, &cmd),
            ctx.name,
            Duration::from_millis(500),
            Duration::from_millis(500),
            3,
        )?;

        if sent {
            tracing::debug!(
                session = ctx.session,
                name = ctx.name,
                "pi /name command sent"
            );
        } else {
            tracing::warn!(
                session = ctx.session,
                name = ctx.name,
                "pi /name command failed after retries; session usable but may display Pi's default name"
            );
        }
        Ok(())
    }

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        super::claude::kill_pane_via_slash(session, window, "/quit", 3)
    }
}

pub fn identity_file_path(state_dir: &Path, session_name: &str) -> PathBuf {
    let safe_name = safe_path_component(session_name);
    state_dir.join("identity").join(format!("{safe_name}.md"))
}

pub fn long_lived_session_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pi-sessions")
}

pub fn task_session_dir(task_dir: &Path, session_name: &str) -> PathBuf {
    task_dir
        .join("pi-sessions")
        .join(safe_path_component(session_name))
}

fn safe_path_component(name: &str) -> String {
    name.replace(['/', '\\'], "-")
}

fn register_session_name_with_retry<F>(
    mut paste_attempt: F,
    name: &str,
    initial_delay: Duration,
    retry_delay: Duration,
    max_attempts: u32,
) -> Result<bool>
where
    F: FnMut() -> Result<()>,
{
    if !initial_delay.is_zero() {
        std::thread::sleep(initial_delay);
    }
    for attempt in 1..=max_attempts {
        match paste_attempt() {
            Ok(()) => return Ok(true),
            Err(e) => {
                tracing::warn!(
                    attempt,
                    name,
                    error = %e,
                    "pi /name: paste attempt failed; will retry"
                );
                if attempt < max_attempts && !retry_delay.is_zero() {
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
    Ok(false)
}

/// Paste `text` into a tmux target as a single block followed by Enter,
/// using load-buffer + paste-buffer so spaces and shell metacharacters
/// survive.
fn paste_text(target: &str, text: &str) -> Result<()> {
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(text.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux load-buffer failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let paste = Command::new("tmux")
        .args(["paste-buffer", "-p", "-t", target])
        .output()?;
    if !paste.status.success() {
        anyhow::bail!(
            "tmux paste-buffer failed: {}",
            String::from_utf8_lossy(&paste.stderr)
        );
    }

    std::thread::sleep(Duration::from_millis(200));
    let enter = Command::new("tmux")
        .args(["send-keys", "-t", target, "Enter"])
        .output()?;
    if !enter.status.success() {
        anyhow::bail!(
            "tmux send-keys (Enter) failed: {}",
            String::from_utf8_lossy(&enter.stderr)
        );
    }
    Ok(())
}
