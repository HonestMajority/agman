//! Pluggable AI agent harness abstraction.
//!
//! agman supports multiple interactive CLI harnesses for hosting agents:
//! - `claude` (Anthropic Claude Code CLI)
//! - `codex`  (OpenAI Codex CLI)
//! - `goose`  (Block Goose CLI)
//! - `pi`     (Pi coding agent CLI)
//!
//! The user picks one in the TUI settings view; the choice is persisted as
//! `harness = "..."` in `~/.agman/config.toml` and applies to every newly
//! spawned agent (CEO, PM, researcher, task agents).
//!
//! Long-lived agents (CEO/PM/researcher) stamp `<state_dir>/harness` on first
//! spawn so a global flip doesn't break in-flight agents. Task agents capture
//! their spawn-time `harness` on `SessionEntry`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub mod claude;
pub mod codex;
pub mod goose;
pub mod pi;

/// Test-only re-export of the codex session_index polling helper. Used by
/// integration tests to verify the polling logic without a running codex.
#[doc(hidden)]
pub fn poll_session_index_for_test(
    index_path: &std::path::Path,
    name: &str,
    timeout: std::time::Duration,
) -> bool {
    codex::poll_session_index_for(index_path, name, timeout)
}

/// Test-only re-export of the codex /rename retry loop. Tests pass a
/// closure-based `paste_attempt` so the retry/poll logic can be verified
/// without running tmux or codex. See `codex::register_session_name_with_retry`
/// for behaviour.
#[doc(hidden)]
pub fn register_session_name_with_retry_for_test(
    paste_attempt: Box<dyn FnMut() -> Result<()> + Send>,
    index_path: &std::path::Path,
    name: &str,
    initial_delay: std::time::Duration,
    poll_timeout: std::time::Duration,
    max_attempts: u32,
) -> Result<bool> {
    codex::register_session_name_with_retry(
        paste_attempt,
        index_path,
        name,
        initial_delay,
        poll_timeout,
        max_attempts,
    )
}

/// Test-only entrypoint that pre-stamps workspace trust against an explicit
/// trust-file path (`.claude.json` for claude, `config.toml` for codex).
/// Avoids the env-var-based dispatch that backs the trait method, so
/// concurrent tests can each point at their own `TempDir` without
/// serializing on a process-global env var.
#[doc(hidden)]
pub fn ensure_workspace_trusted_for_test(
    kind: HarnessKind,
    trust_file: &Path,
    cwd: &Path,
) -> Result<()> {
    match kind {
        HarnessKind::Claude => claude::ensure_workspace_trusted_in(trust_file, cwd),
        HarnessKind::Codex => codex::ensure_workspace_trusted_in(trust_file, cwd),
        HarnessKind::Goose => Ok(()),
        HarnessKind::Pi => Ok(()),
    }
}

/// Test-only entrypoint for codex browser MCP configuration. Production uses
/// `CodexHarness::ensure_capabilities_configured`, which resolves the config
/// path from `harness_home(Codex)`.
#[doc(hidden)]
pub fn ensure_browser_mcp_for_test(config_toml_path: &Path) -> Result<()> {
    codex::ensure_browser_mcp_in(config_toml_path)
}

/// Identifies which harness to use. Persisted in config + per-agent stamps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    #[default]
    Claude,
    Codex,
    Goose,
    Pi,
}

impl HarnessKind {
    pub const ALL: &'static [Self] = &[Self::Claude, Self::Codex, Self::Goose, Self::Pi];

    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessKind::Claude => "claude",
            HarnessKind::Codex => "codex",
            HarnessKind::Goose => "goose",
            HarnessKind::Pi => "pi",
        }
    }

    /// Materialize the trait object. Cheap — harness impls are zero-sized.
    pub fn select(self) -> Box<dyn Harness> {
        match self {
            HarnessKind::Claude => Box::new(claude::ClaudeHarness),
            HarnessKind::Codex => Box::new(codex::CodexHarness),
            HarnessKind::Goose => Box::new(goose::GooseHarness),
            HarnessKind::Pi => Box::new(pi::PiHarness),
        }
    }
}

impl FromStr for HarnessKind {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "goose" => Ok(Self::Goose),
            "pi" => Ok(Self::Pi),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Read a per-agent harness stamp from `<state_dir>/harness`. If the file is
/// absent, write the supplied `default_kind` so subsequent calls are stable
/// even after a global setting flip.
pub fn read_or_stamp(state_dir: &Path, default_kind: HarnessKind) -> Result<HarnessKind> {
    let stamp = state_dir.join("harness");
    if stamp.exists() {
        let raw = std::fs::read_to_string(&stamp).unwrap_or_default();
        return Ok(raw.trim().parse().unwrap_or(default_kind));
    }
    if let Some(parent) = stamp.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::write(&stamp, default_kind.as_str());
    Ok(default_kind)
}

/// How the upcoming launch maps to a harness session id/name.
///
/// Task agents always use `Auto` (every flow step is a fresh session;
/// agman never resumes them). Long-lived agents (CEO/PM/researcher) use
/// `Pin` on first launch and `Resume` on subsequent launches.
///
/// The `'a` lifetime borrows the stamped session-id (claude) or
/// stamped unique session name (codex/goose/pi) from the caller.
#[derive(Debug, Clone, Copy)]
pub enum SessionKey<'a> {
    /// No resume, no pin. Claude and goose receive the launch name directly;
    /// codex/pi may be renamed post-launch via `/rename` or `/name`.
    Auto,
    /// First launch of a long-lived agent. Claude pins the supplied UUID
    /// via `--session-id <uuid>`. Codex/goose/pi have no launch-time session-id
    /// pin and treat this like `Auto`.
    Pin(&'a str),
    /// Resume an existing long-lived session. Claude resumes by UUID
    /// (`--resume <uuid>`); codex/goose resume by stamped unique name;
    /// pi resumes latest session in a private session dir via `--continue`.
    Resume(&'a str),
}

/// Optional assistant capabilities requested at launch time.
#[derive(Default, Clone, Copy, Debug)]
pub struct AssistantCapabilities {
    pub browser: bool,
}

/// Static input for `Harness::build_session_command`. Names follow the
/// harness's resume / session-listing convention so the user can reattach
/// manually from a shell (`claude --resume <id>`, `codex resume <name>`, or
/// `goose session --resume --name <name>`; pi uses its private session dir).
pub struct LaunchContext<'a> {
    /// Inline system-prompt body. Passed to claude via `--system-prompt` and
    /// to codex via `-c 'developer_instructions="""..."""'`. Skipped on
    /// `SessionKey::Resume` for claude/codex (the resumed thread already
    /// has its system prompt baked in). Harnesses that need a file receive
    /// the same body through `identity_file`.
    pub identity: &'a str,
    /// Session name passed to the harness. Long-lived agents use a stamped
    /// unique generation name; task agents use a unique step name.
    pub name: &'a str,
    /// Harness-specific identity file. Goose passes this via
    /// `GOOSE_MOIM_MESSAGE_FILE`; pi passes this via
    /// `--append-system-prompt`.
    pub identity_file: Option<&'a Path>,
    /// Harness-specific private session directory. Pi passes this via
    /// `--session-dir`; other harnesses ignore it.
    pub session_dir: Option<&'a Path>,
    /// Working directory for the launched process.
    pub cwd: &'a Path,
    /// Codex-only: pass `--no-alt-screen` so `tmux capture-pane` can read
    /// pane content (needed by the inbox snippet-verification loop). Claude
    /// ignores.
    pub no_alt_screen: bool,
    /// Optional assistant capabilities. Non-assistant launches pass default.
    pub capabilities: AssistantCapabilities,
    /// Whether this launch pins a fresh session, resumes a prior one, or
    /// neither. See `SessionKey` for the per-variant behaviour.
    pub session_key: SessionKey<'a>,
}

/// Static input for `Harness::register_session_name`. Used by codex's
/// post-launch `/rename <name>` step and pi's `/name <name>` step.
/// Claude/goose no-op.
pub struct RegisterContext<'a> {
    pub session: &'a str,
    pub window: Option<&'a str>,
    pub name: &'a str,
    /// Harness home dir (`~/.codex` for codex, `~/.claude` for claude, etc.).
    pub harness_home: &'a Path,
}

/// Behaviour exposed by a harness. Add new harnesses by adding a
/// `HarnessKind` variant and
/// a corresponding impl.
pub trait Harness: Send + Sync {
    fn kind(&self) -> HarnessKind;
    fn cli_binary(&self) -> &'static str;
    fn install_hint(&self) -> &'static str;
    fn skill_hint(&self) -> &'static str;

    /// Build the shell command string handed to `tmux send-keys`.
    fn build_session_command(&self, ctx: &LaunchContext) -> String;

    /// Ensure `cwd` is registered as a trusted workspace in the harness's
    /// user-global config so the interactive trust dialog does not block the
    /// launch. Idempotent. Tolerates missing config file/dir (creates as
    /// needed). MUST NOT clobber other entries in the same config file —
    /// claude in particular stores many per-project keys we don't own.
    ///
    /// SIDE EFFECT — kept distinct from `build_session_command` (which is a
    /// pure command-string builder). Call sites must invoke this before
    /// sending the launch command to tmux.
    ///
    /// Failing the trust-stamp fails the launch: a launch that hits the
    /// trust dialog is unrecoverable for agman (the agent never reaches a
    /// usable state and `/rename` paste-injects run as shell commands).
    fn ensure_workspace_trusted(&self, cwd: &Path) -> Result<()>;

    /// Ensure any requested assistant capabilities are configured before
    /// launch. Harnesses that do not need setup use the default no-op.
    fn ensure_capabilities_configured(&self, _caps: &AssistantCapabilities) -> Result<()> {
        Ok(())
    }

    /// Post-launch step. Called once `is_session_ready_in` reports the
    /// foreground process is no longer a shell.
    /// - Claude: no-op.
    /// - Codex: paste `/rename <name>` + Enter, then verify by tailing
    ///   `~/.codex/session_index.jsonl` for ≤ 5s. On timeout: log warning
    ///   and return Ok — the session is still usable, just not
    ///   resume-by-name.
    /// - Goose: no-op.
    /// - Pi: best-effort paste `/name <name>` + Enter.
    fn register_session_name(&self, ctx: &RegisterContext) -> Result<()>;

    /// Tear down the foreground agent in a tmux pane gracefully.
    /// - Claude: `/exit` + Enter, fallback Ctrl-C × 2.
    /// - Codex:  `/quit` + Enter, fallback Ctrl-C × 3.
    /// - Goose:  `/exit`, Enter in a separate tmux call, fallback Ctrl-C × 3.
    /// - Pi:     `/quit` + Enter, fallback Ctrl-C × 3.
    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()>;
}

/// Resolve the home directory for a harness, honoring optional env overrides
/// used by tests. `AGMAN_CLAUDE_HOME` overrides the claude home; similarly
/// for other harnesses.
pub fn harness_home(kind: HarnessKind) -> PathBuf {
    let env_var = match kind {
        HarnessKind::Claude => "AGMAN_CLAUDE_HOME",
        HarnessKind::Codex => "AGMAN_CODEX_HOME",
        HarnessKind::Goose => "AGMAN_GOOSE_HOME",
        HarnessKind::Pi => "AGMAN_PI_HOME",
    };
    if let Ok(dir) = std::env::var(env_var) {
        return PathBuf::from(dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    match kind {
        HarnessKind::Claude => home.join(".claude"),
        HarnessKind::Codex => home.join(".codex"),
        HarnessKind::Goose => home.join(".local").join("share").join("goose"),
        HarnessKind::Pi => home.join(".pi").join("agent"),
    }
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
