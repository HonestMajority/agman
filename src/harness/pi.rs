//! Pi harness implementation.

use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{Harness, HarnessKind, LaunchContext, RegisterContext, SessionKey};

const TOOL_ALLOWLIST: &str = "read,bash,edit,write,grep,find,ls";
const NAME_REGISTRATION_INITIAL_DELAY: Duration = Duration::from_millis(500);
const NAME_REGISTRATION_RETRY_DELAY: Duration = Duration::from_millis(500);
const NAME_REGISTRATION_SUBMIT_DELAY: Duration = Duration::from_millis(500);
const NAME_REGISTRATION_SECOND_SUBMIT_DELAY: Duration = Duration::from_millis(120);
const NAME_REGISTRATION_PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(10);
const NAME_REGISTRATION_PROMPT_READY_POLL_DELAY: Duration = Duration::from_millis(200);
const NAME_REGISTRATION_MAX_ATTEMPTS: u32 = 3;
const PI_SUBMIT_CR_HEX: &str = "0d";

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

        if wait_for_prompt_ready(&target, NAME_REGISTRATION_PROMPT_READY_TIMEOUT) {
            tracing::debug!(
                session = ctx.session,
                name = ctx.name,
                "pi prompt ready before /name registration"
            );
        } else {
            tracing::warn!(
                session = ctx.session,
                name = ctx.name,
                "pi prompt-ready cue not observed before /name registration; continuing best-effort"
            );
        }

        let sent = register_session_name_with_retry(
            || submit_slash_command(&target, &cmd),
            ctx.name,
            NAME_REGISTRATION_INITIAL_DELAY,
            NAME_REGISTRATION_RETRY_DELAY,
            NAME_REGISTRATION_MAX_ATTEMPTS,
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

fn wait_for_prompt_ready(target: &str, timeout: Duration) -> bool {
    wait_for_prompt_ready_with_runner(
        target,
        timeout,
        NAME_REGISTRATION_PROMPT_READY_POLL_DELAY,
        capture_tmux_pane_tail,
    )
}

fn wait_for_prompt_ready_with_runner<F>(
    target: &str,
    timeout: Duration,
    poll_delay: Duration,
    mut capture: F,
) -> bool
where
    F: FnMut(&str) -> Result<String>,
{
    let deadline = Instant::now() + timeout;
    loop {
        match capture(target) {
            Ok(text) if pane_text_has_prompt_ready_cue(&text) => return true,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(
                    target,
                    error = %e,
                    "pi prompt readiness capture failed; will retry until timeout"
                );
            }
        }

        if Instant::now() >= deadline {
            return false;
        }
        if !poll_delay.is_zero() {
            std::thread::sleep(poll_delay);
        }
    }
}

fn pane_text_has_prompt_ready_cue(text: &str) -> bool {
    text.contains("/ commands") && text.contains("ctrl+o")
}

fn register_session_name_with_retry<F>(
    mut submit_attempt: F,
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
        match submit_attempt() {
            Ok(()) => return Ok(true),
            Err(e) => {
                tracing::warn!(
                    attempt,
                    name,
                    error = %e,
                    "pi /name: submit attempt failed; will retry"
                );
                if attempt < max_attempts && !retry_delay.is_zero() {
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
    Ok(false)
}

fn submit_slash_command(target: &str, text: &str) -> Result<()> {
    submit_slash_command_with_runner(
        target,
        text,
        NAME_REGISTRATION_SUBMIT_DELAY,
        NAME_REGISTRATION_SECOND_SUBMIT_DELAY,
        run_tmux_submit_action,
    )
}

/// Submit Pi slash commands with a raw paste and delayed carriage returns.
///
/// Pi's prompt can leave bracket-pasted slash commands in the editor with
/// blank continuation lines. The command text is single-line, so raw tmux
/// paste is sufficient and avoids bracket-paste editor state. Once the editor
/// is mounted, raw carriage return via tmux hex mode matches the sequence
/// verified against Pi's TUI; the second CR preserves the prior reliability
/// guard.
fn submit_slash_command_with_runner<F>(
    target: &str,
    text: &str,
    submit_delay: Duration,
    second_submit_delay: Duration,
    mut run: F,
) -> Result<()>
where
    F: FnMut(PiSubmitAction) -> Result<()>,
{
    run(PiSubmitAction::LoadBuffer(text.to_string()))?;
    run(PiSubmitAction::PasteBufferRaw {
        target: target.to_string(),
    })?;

    if !submit_delay.is_zero() {
        std::thread::sleep(submit_delay);
    }
    run(PiSubmitAction::SendHex {
        target: target.to_string(),
        hex: PI_SUBMIT_CR_HEX,
    })?;

    if !second_submit_delay.is_zero() {
        std::thread::sleep(second_submit_delay);
    }
    run(PiSubmitAction::SendHex {
        target: target.to_string(),
        hex: PI_SUBMIT_CR_HEX,
    })?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PiSubmitAction {
    LoadBuffer(String),
    PasteBufferRaw { target: String },
    SendHex { target: String, hex: &'static str },
}

fn run_tmux_submit_action(action: PiSubmitAction) -> Result<()> {
    match action {
        PiSubmitAction::LoadBuffer(text) => load_tmux_buffer(&text),
        PiSubmitAction::PasteBufferRaw { target } => paste_tmux_buffer_raw(&target),
        PiSubmitAction::SendHex { target, hex } => send_tmux_hex(&target, hex),
    }
}

fn capture_tmux_pane_tail(target: &str) -> Result<String> {
    let capture = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", target, "-S", "-80"])
        .output()?;
    if !capture.status.success() {
        anyhow::bail!(
            "tmux capture-pane failed: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&capture.stdout).to_string())
}

fn load_tmux_buffer(text: &str) -> Result<()> {
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
    Ok(())
}

fn paste_tmux_buffer_raw(target: &str) -> Result<()> {
    let paste = Command::new("tmux")
        .args(["paste-buffer", "-t", target])
        .output()?;
    if !paste.status.success() {
        anyhow::bail!(
            "tmux paste-buffer failed: {}",
            String::from_utf8_lossy(&paste.stderr)
        );
    }
    Ok(())
}

fn send_tmux_hex(target: &str, hex: &str) -> Result<()> {
    let send = Command::new("tmux")
        .args(["send-keys", "-H", "-t", target, hex])
        .output()?;
    if !send.status.success() {
        anyhow::bail!(
            "tmux send-keys -H ({hex}) failed: {}",
            String::from_utf8_lossy(&send.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn pi_prompt_ready_requires_commands_and_ctrl_o() {
        assert!(!pane_text_has_prompt_ready_cue("pi v0.73.0\n/ commands"));
        assert!(!pane_text_has_prompt_ready_cue("pi v0.73.0\nctrl+o"));
        assert!(pane_text_has_prompt_ready_cue(
            "pi v0.73.0\n/ commands\nctrl+o"
        ));
    }

    #[test]
    fn pi_wait_for_prompt_ready_polls_until_ready_cue() {
        let captures = Arc::new(Mutex::new(vec![
            "pane_current_command=node".to_string(),
            "pi v0.73.0\n/ commands".to_string(),
            "pi v0.73.0\n/ commands\nctrl+o".to_string(),
        ]));
        let captures_in_runner = Arc::clone(&captures);
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_in_runner = Arc::clone(&attempts);

        let ready = wait_for_prompt_ready_with_runner(
            "agman-test:agman",
            Duration::from_secs(1),
            Duration::ZERO,
            move |target| {
                assert_eq!(target, "agman-test:agman");
                attempts_in_runner.fetch_add(1, Ordering::SeqCst);
                Ok(captures_in_runner.lock().unwrap().remove(0))
            },
        );

        assert!(ready);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn pi_wait_for_prompt_ready_times_out_best_effort() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_in_runner = Arc::clone(&attempts);

        let ready = wait_for_prompt_ready_with_runner(
            "agman-test:agman",
            Duration::ZERO,
            Duration::ZERO,
            move |_| {
                attempts_in_runner.fetch_add(1, Ordering::SeqCst);
                Ok("pi v0.73.0\n/ commands".to_string())
            },
        );

        assert!(!ready);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pi_submit_slash_command_uses_raw_paste_and_double_hex_cr() {
        let actions = Arc::new(Mutex::new(Vec::new()));
        let actions_in_runner = Arc::clone(&actions);

        submit_slash_command_with_runner(
            "agman-test:agman",
            "/name agman-task-test-step-1",
            Duration::ZERO,
            Duration::ZERO,
            move |action| {
                actions_in_runner.lock().unwrap().push(action);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            actions.lock().unwrap().as_slice(),
            &[
                PiSubmitAction::LoadBuffer("/name agman-task-test-step-1".to_string()),
                PiSubmitAction::PasteBufferRaw {
                    target: "agman-test:agman".to_string()
                },
                PiSubmitAction::SendHex {
                    target: "agman-test:agman".to_string(),
                    hex: "0d"
                },
                PiSubmitAction::SendHex {
                    target: "agman-test:agman".to_string(),
                    hex: "0d"
                },
            ]
        );
    }

    #[test]
    fn pi_register_session_name_retries_submit_failures() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_in_submit = Arc::clone(&attempts);

        let result = register_session_name_with_retry(
            move || {
                let n = attempts_in_submit.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    Err(anyhow!("tmux not ready"))
                } else {
                    Ok(())
                }
            },
            "agman-task-test-step-1",
            Duration::ZERO,
            Duration::ZERO,
            3,
        )
        .unwrap();

        assert!(result);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
