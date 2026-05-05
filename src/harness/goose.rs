//! Goose harness implementation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Harness, HarnessKind, LaunchContext, RegisterContext, SessionKey};
use crate::tmux::Tmux;

pub struct GooseHarness;

impl Harness for GooseHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Goose
    }

    fn cli_binary(&self) -> &'static str {
        "goose"
    }

    fn install_hint(&self) -> &'static str {
        "Install the Goose CLI and ensure `goose` is on PATH"
    }

    fn skill_hint(&self) -> &'static str {
        ""
    }

    fn build_session_command(&self, ctx: &LaunchContext) -> String {
        let identity_file = ctx
            .identity_file
            .expect("goose launches require LaunchContext::identity_file");
        let identity_file = super::shell_single_quote(&identity_file.to_string_lossy());
        let name = super::shell_single_quote(ctx.name);

        let mut cmd = format!(
            "GOOSE_MODE=auto GOOSE_MOIM_MESSAGE_FILE={} goose session --with-builtin developer,tom",
            identity_file
        );
        if matches!(ctx.session_key, SessionKey::Resume(_)) {
            cmd.push_str(" --resume");
        }
        cmd.push_str(&format!(" --name {}", name));
        cmd
    }

    fn ensure_workspace_trusted(&self, _cwd: &Path) -> Result<()> {
        Ok(())
    }

    fn register_session_name(&self, _ctx: &RegisterContext) -> Result<()> {
        Ok(())
    }

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        kill_pane_via_separate_enter(session, window, "/exit", 3)
    }
}

pub fn identity_file_path(state_dir: &Path, session_name: &str) -> PathBuf {
    let safe_name = session_name.replace(['/', '\\'], "-");
    state_dir.join("identity").join(format!("{safe_name}.md"))
}

fn kill_pane_via_separate_enter(
    session: &str,
    window: Option<&str>,
    slash: &str,
    ctrl_c_count: usize,
) -> Result<()> {
    let still_running = match Tmux::is_agent_running_in(session, window) {
        Ok((running, _)) => running,
        Err(e) => {
            tracing::debug!(
                session = session,
                error = %e,
                "goose kill_pane: pane not queryable; nothing to kill"
            );
            return Ok(());
        }
    };
    if !still_running {
        tracing::debug!(
            session = session,
            "goose kill_pane: pane already at shell; no-op"
        );
        return Ok(());
    }

    tracing::debug!(
        session = session,
        slash,
        "goose kill_pane: sending slash command"
    );
    let send_result = match window {
        Some(w) => Tmux::send_text_to_window(session, w, slash)
            .and_then(|_| Tmux::send_enter_to(session, Some(w))),
        None => Tmux::send_text_to_session(session, slash).and_then(|_| Tmux::send_enter(session)),
    };

    match send_result {
        Ok(()) => {
            if super::claude::wait_for_pane_idle(session, window, Duration::from_secs(5)) {
                return Ok(());
            }
            tracing::warn!(
                session = session,
                slash,
                "goose slash command did not return pane to shell within 5s; escalating to Ctrl-C"
            );
        }
        Err(e) => {
            tracing::warn!(
                session = session,
                slash,
                error = %e,
                "failed to send goose slash command; escalating to Ctrl-C"
            );
        }
    }

    for i in 0..ctrl_c_count {
        if i > 0 && super::claude::wait_for_pane_idle(session, window, Duration::from_millis(150)) {
            return Ok(());
        }
        let _ = match window {
            Some(w) => Tmux::send_ctrl_c_to_window(session, w),
            None => Tmux::send_ctrl_c_to_session(session),
        };
        std::thread::sleep(Duration::from_millis(150));
    }

    if super::claude::wait_for_pane_idle(session, window, Duration::from_secs(2)) {
        return Ok(());
    }

    anyhow::bail!(
        "failed to quit goose agent in pane '{}:{}' after {} + Ctrl-C x{}",
        session,
        window.unwrap_or(""),
        slash,
        ctrl_c_count
    )
}
