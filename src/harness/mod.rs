//! Pluggable AI agent harness abstraction.
//!
//! agman supports multiple interactive CLI harnesses for hosting agents:
//! - `claude` (Anthropic Claude Code CLI)
//! - `codex`  (OpenAI Codex CLI)
//!
//! The user picks one in the TUI settings view; the choice is persisted as
//! `harness = "..."` in `~/.agman/config.toml` and applies to every newly
//! spawned agent (CEO, PM, researcher, task agents).
//!
//! Long-lived agents (CEO/PM/researcher) stamp `<state_dir>/harness` on first
//! spawn so a global flip doesn't break in-flight agents. Task agents capture
//! `harness` on `TaskMeta` at task-create time.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod claude;
pub mod codex;

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

/// Test-only entrypoint that resolves the most-recently-modified transcript
/// for `cwd` under an explicit `home` directory. Avoids the env-var-based
/// dispatch that backs the trait method, so concurrent tests can each point
/// at their own `TempDir` without serializing on a process-global env var.
#[doc(hidden)]
pub fn latest_transcript_for_test(kind: HarnessKind, home: &Path, cwd: &Path) -> Option<PathBuf> {
    match kind {
        HarnessKind::Claude => claude::latest_transcript_in(home, cwd),
        HarnessKind::Codex => codex::latest_transcript_in(home, cwd),
    }
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
    }
}

/// Identifies which harness to use. Persisted in config + per-agent stamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Claude,
    Codex,
}

impl HarnessKind {
    pub const ALL: &'static [Self] = &[Self::Claude, Self::Codex];

    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessKind::Claude => "claude",
            HarnessKind::Codex => "codex",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Materialize the trait object. Cheap — both impls are zero-sized.
    pub fn select(self) -> Box<dyn Harness> {
        match self {
            HarnessKind::Claude => Box::new(claude::ClaudeHarness),
            HarnessKind::Codex => Box::new(codex::CodexHarness),
        }
    }
}

impl Default for HarnessKind {
    fn default() -> Self {
        HarnessKind::Claude
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
        return Ok(HarnessKind::from_str(raw.trim()).unwrap_or(default_kind));
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
/// deterministic name (codex) from the caller.
#[derive(Debug, Clone, Copy)]
pub enum SessionKey<'a> {
    /// No resume, no pin. Claude generates its own UUID; codex auto-names
    /// (and may be renamed post-launch via `/rename`).
    Auto,
    /// First launch of a long-lived agent. Claude pins the supplied UUID
    /// via `--session-id <uuid>`. Codex has no launch-time pin and treats
    /// this like `Auto` (the deterministic name is registered post-launch).
    Pin(&'a str),
    /// Resume an existing long-lived session. Claude resumes by UUID
    /// (`--resume <uuid>`); codex resumes by deterministic name
    /// (`codex resume <name>`).
    Resume(&'a str),
}

/// Static input for `Harness::build_session_command`. Names follow the
/// harness's resume / session-listing convention so the user can reattach
/// manually from a shell (`claude --resume <name>` / `codex resume <name>`).
pub struct LaunchContext<'a> {
    /// Inline system-prompt body. Passed to claude via `--system-prompt` and
    /// to codex via `-c 'developer_instructions="""..."""'`. Skipped on
    /// `SessionKey::Resume` for both harnesses (the resumed thread already
    /// has its system prompt baked in).
    pub identity: &'a str,
    /// Stable, deterministic session name (e.g. `agman-chief-of-staff`,
    /// `agman-pm-<project>`, `agman-r-<project>--<name>`,
    /// `agman-task-<task-id>-step-<n>`).
    pub name: &'a str,
    /// Working directory for the launched process.
    pub cwd: &'a Path,
    /// Codex-only: pass `--no-alt-screen` so `tmux capture-pane` can read
    /// pane content (needed by the inbox snippet-verification loop). Claude
    /// ignores.
    pub no_alt_screen: bool,
    /// Whether this launch pins a fresh session, resumes a prior one, or
    /// neither. See `SessionKey` for the per-variant behaviour.
    pub session_key: SessionKey<'a>,
}

/// Static input for `Harness::register_session_name`. Used by codex's
/// post-launch `/rename <name>` step. Claude no-ops.
pub struct RegisterContext<'a> {
    pub session: &'a str,
    pub window: Option<&'a str>,
    pub name: &'a str,
    /// Harness home dir (`~/.codex` for codex, `~/.claude` for claude).
    pub harness_home: &'a Path,
}

/// Behaviour exposed by a harness. Two impls today: `ClaudeHarness` and
/// `CodexHarness`. Add new harnesses by adding a `HarnessKind` variant and
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

    /// Post-launch step. Called once `is_session_ready_in` reports the
    /// foreground process is no longer a shell.
    /// - Claude: no-op.
    /// - Codex: paste `/rename <name>` + Enter, then verify by tailing
    ///   `~/.codex/session_index.jsonl` for ≤ 5s. On timeout: log warning
    ///   and return Ok — the session is still usable, just not
    ///   resume-by-name.
    fn register_session_name(&self, ctx: &RegisterContext) -> Result<()>;

    /// Tear down the foreground agent in a tmux pane gracefully.
    /// - Claude: `/exit` + Enter, fallback Ctrl-C × 2.
    /// - Codex:  `/quit` + Enter, fallback Ctrl-C × 3.
    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()>;

    /// Most-recently-modified transcript file matching the agent's cwd.
    /// - Claude: `~/.claude/projects/<escaped-cwd>/*.jsonl` by mtime.
    /// - Codex:  walks `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`,
    ///   matching the first-line `session_meta.payload.cwd` field.
    fn latest_transcript(&self, cwd: &Path) -> Option<PathBuf>;

    /// Stable marker for the last assistant message in a transcript.
    /// - Claude: last `type=="assistant"` entry's `uuid`.
    /// - Codex:  last `event_msg{type=agent_message}` entry's `timestamp`.
    fn find_last_assistant_marker(&self, transcript: &Path) -> Option<String>;
}

/// Resolve the home directory for a harness, honoring optional env overrides
/// used by tests. `AGMAN_CLAUDE_HOME` overrides the claude home; similarly
/// for codex.
pub fn harness_home(kind: HarnessKind) -> PathBuf {
    let env_var = match kind {
        HarnessKind::Claude => "AGMAN_CLAUDE_HOME",
        HarnessKind::Codex => "AGMAN_CODEX_HOME",
    };
    if let Ok(dir) = std::env::var(env_var) {
        return PathBuf::from(dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    match kind {
        HarnessKind::Claude => home.join(".claude"),
        HarnessKind::Codex => home.join(".codex"),
    }
}
