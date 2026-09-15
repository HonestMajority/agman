//! Codex (OpenAI Codex CLI) harness implementation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{AgentCapabilities, Harness, HarnessKind, LaunchContext, RegisterContext, SessionKey};

pub struct CodexHarness;

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn cli_binary(&self) -> &'static str {
        "codex"
    }

    fn install_hint(&self) -> &'static str {
        "brew install --cask codex (macOS) / npm install -g @openai/codex"
    }

    fn skill_hint(&self) -> &'static str {
        // Per locked decision: skip the skill hint for codex.
        ""
    }

    fn build_session_command(&self, ctx: &LaunchContext) -> String {
        // Resume short-circuits: `codex resume <uuid>` keeps the saved
        // thread's developer_instructions, so we skip the `-c ...` arg.
        // The handle is the session UUID persisted in `<state_dir>/session-id`
        // (see `resolve_session_id`); codex no longer accepts a bare label
        // when it cannot prove the label unique across its server pages.
        // Pass the working directory via `-C <cwd>` so codex doesn't
        // prompt a directory picker when launch cwd differs from saved.
        if let SessionKey::Resume(session_id) = ctx.session_key {
            let cwd_str = ctx.cwd.to_string_lossy().replace('\'', "'\\''");
            let escaped_id = session_id.replace('\'', "'\\''");
            let mut cmd = String::from("codex");
            // Always run codex with full approval+sandbox bypass. Mirrors
            // claude's `--dangerously-skip-permissions`. Without this, codex
            // prompts before privileged-feeling shell commands (`git add`,
            // etc.), which deadlocks autonomous agman agents.
            cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
            if ctx.no_alt_screen {
                cmd.push_str(" --no-alt-screen");
            }
            if ctx.capabilities.browser {
                cmd.push_str(" -c 'mcp_servers.playwright.enabled=true'");
            }
            cmd.push_str(&format!(" -C '{}'", cwd_str));
            cmd.push_str(&format!(" resume '{}'", escaped_id));
            return cmd;
        }

        // Auto and Pin: identical fresh-launch shape. Codex doesn't accept
        // a launch-time session-id pin; the deterministic name is
        // registered post-launch via `/rename`.
        // Codex consumes identity via TOML `developer_instructions`. Use
        // triple-quoted strings so newlines are preserved verbatim. Defensive
        // escape literal `"""` in the body.
        let body = ctx.identity.replace("\"\"\"", "\\\"\\\"\\\"");
        let dev_instructions = format!("developer_instructions=\"\"\"{}\"\"\"", body);
        // Single-quote the whole `-c` arg, escaping inner single quotes.
        let dev_arg_escaped = dev_instructions.replace('\'', "'\\''");

        let mut cmd = String::from("codex");
        // Always run codex with full approval+sandbox bypass. Mirrors
        // claude's `--dangerously-skip-permissions`. Without this, codex
        // prompts before privileged-feeling shell commands (`git add`, etc.),
        // which deadlocks autonomous agman agents.
        cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
        if ctx.no_alt_screen {
            cmd.push_str(" --no-alt-screen");
        }
        cmd.push_str(&format!(" -c '{}'", dev_arg_escaped));
        if ctx.capabilities.browser {
            cmd.push_str(" -c 'mcp_servers.playwright.enabled=true'");
        }
        cmd
    }

    /// Pre-stamp workspace trust in `~/.codex/config.toml` so the
    /// interactive trust dialog does not block first launch in `cwd`.
    fn ensure_workspace_trusted(&self, cwd: &Path) -> Result<()> {
        let trust_file = super::harness_home(HarnessKind::Codex).join("config.toml");
        ensure_workspace_trusted_in(&trust_file, cwd)
    }

    fn ensure_capabilities_configured(&self, caps: &AgentCapabilities) -> Result<()> {
        if caps.browser {
            let config_toml = super::harness_home(HarnessKind::Codex).join("config.toml");
            ensure_browser_mcp_in(&config_toml)?;
        }
        Ok(())
    }

    /// Paste-inject `/rename <name>` post-launch and verify the entry shows
    /// up in `~/.codex/session_index*.jsonl`. Self-verifying with retry: codex
    /// step 2+ relaunches faster than first launch (file watchers warm, no
    /// first-time prompts), so the bracket-paste handler isn't always
    /// fully mounted when `wait_for_agent_ready` returns. Sleep ~500 ms,
    /// then loop up to 3 attempts of `paste + 2 s poll`. On all-three timeout,
    /// log a warning and return Ok — the session is still usable, just not
    /// resume-by-name.
    fn register_session_name(&self, ctx: &RegisterContext) -> Result<()> {
        let target = match ctx.window {
            Some(w) => format!("{}:{}", ctx.session, w),
            None => ctx.session.to_string(),
        };
        let cmd = format!("/rename {}", ctx.name);

        let found = register_session_name_with_retry(
            || paste_text(&target, &cmd),
            ctx.harness_home,
            ctx.name,
            Duration::from_millis(500),
            Duration::from_secs(2),
            3,
        )?;

        if found {
            tracing::debug!(
                session = ctx.session,
                name = ctx.name,
                "codex /rename registered in session index"
            );
        } else {
            tracing::warn!(
                session = ctx.session,
                name = ctx.name,
                harness_home = %ctx.harness_home.display(),
                "codex /rename did not appear in session_index*.jsonl after 3 retries; session usable but agman cannot resolve its UUID for resume"
            );
        }
        Ok(())
    }

    fn resolve_session_id(&self, harness_home: &Path, name: &str) -> Result<Option<String>> {
        resolve_session_id(harness_home, name).map(Some)
    }

    fn kill_pane(&self, session: &str, window: Option<&str>) -> Result<()> {
        super::claude::kill_pane_via_slash(session, window, "/quit", 3)
    }
}

/// Run `paste_attempt` then poll the session index under `codex_home` for
/// `name`, retrying up to
/// `max_attempts` times. Returns `Ok(true)` if the entry appears within any
/// attempt's poll window, `Ok(false)` if all attempts time out.
///
/// `initial_delay` is slept before the first paste attempt — codex's
/// bracket-paste input handler is not always mounted at the moment
/// `wait_for_agent_ready` returns true (especially on step 2+ relaunches
/// where everything is hot). The delay gives the TUI time to wire it up.
///
/// Per-attempt: paste failures are logged at warn but do NOT short-circuit
/// the loop — the next attempt may succeed (e.g., transient tmux race).
/// Inter-attempt: no extra sleep beyond the polling timeout, which
/// effectively backs off naturally.
pub(crate) fn register_session_name_with_retry<F>(
    mut paste_attempt: F,
    codex_home: &Path,
    name: &str,
    initial_delay: Duration,
    poll_timeout: Duration,
    max_attempts: u32,
) -> anyhow::Result<bool>
where
    F: FnMut() -> anyhow::Result<()>,
{
    if !initial_delay.is_zero() {
        std::thread::sleep(initial_delay);
    }
    for attempt in 1..=max_attempts {
        if let Err(e) = paste_attempt() {
            tracing::warn!(
                attempt,
                name = name,
                error = %e,
                "codex /rename: paste attempt failed; will retry"
            );
        }
        if poll_session_index_for(codex_home, name, poll_timeout) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Poll the session index under `codex_home` for at most `timeout` looking
/// for any entry named `name`. Returns true if such an entry is observed;
/// false on timeout.
pub(crate) fn poll_session_index_for(codex_home: &Path, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut last_size: u64 = 0;
    while Instant::now() < deadline {
        let files = session_index_files(codex_home);
        let size = files
            .iter()
            .filter_map(|f| std::fs::metadata(f).ok())
            .map(|m| m.len())
            .sum();
        if size != last_size {
            if index_entries_named(&files, name).next().is_some() {
                return true;
            }
            last_size = size;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// One `session_index*.jsonl` line: codex writes flat
/// `{"id": <uuid>, "thread_name": <name>, "updated_at": <rfc3339>}` and
/// appends a new line on every rename/update, so a name usually has several
/// lines sharing one id.
#[derive(Debug)]
struct IndexEntry {
    id: String,
    name: String,
    updated_at: Option<String>,
}

impl IndexEntry {
    fn parse(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let obj = v.as_object()?;
        let id = obj.get("id")?.as_str()?.to_string();
        // Codex writes `thread_name`; accept `name` as a forward-compat alias.
        let name = obj
            .get("thread_name")
            .or_else(|| obj.get("name"))?
            .as_str()?
            .to_string();
        let updated_at = obj
            .get("updated_at")
            .and_then(|u| u.as_str())
            .map(str::to_string);
        Some(Self {
            id,
            name,
            updated_at,
        })
    }
}

/// All `session_index*.jsonl` files directly under `codex_home`, sorted by
/// path. Codex currently writes a single `session_index.jsonl`; the glob keeps
/// agman working if it ever versions the file like its sqlite stores.
pub fn session_index_files(codex_home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(codex_home) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("session_index") && n.ends_with(".jsonl"))
        })
        .collect();
    files.sort();
    files
}

fn index_entries_named<'a>(
    files: &'a [PathBuf],
    name: &'a str,
) -> impl Iterator<Item = IndexEntry> + 'a {
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|content| {
            content
                .lines()
                .filter_map(IndexEntry::parse)
                .collect::<Vec<_>>()
        })
        .filter(move |e| e.name == name)
}

/// Resolve the codex session UUID registered under `name` from the session
/// index under `codex_home`.
///
/// Succeeds only when the label maps to exactly one UUID. Zero matches (the
/// `/rename` never landed, or the index was pruned) and several distinct ids
/// (the label was reused) both fail with the label and the candidates so the
/// caller can surface them instead of handing codex a label it will reject.
pub fn resolve_session_id(codex_home: &Path, name: &str) -> Result<String> {
    let files = session_index_files(codex_home);
    // Latest `updated_at` per distinct id, in first-seen order.
    let mut candidates: Vec<IndexEntry> = Vec::new();
    for entry in index_entries_named(&files, name) {
        match candidates.iter_mut().find(|c| c.id == entry.id) {
            Some(existing) => {
                if entry.updated_at > existing.updated_at {
                    existing.updated_at = entry.updated_at;
                }
            }
            None => candidates.push(entry),
        }
    }

    match candidates.as_slice() {
        [] => anyhow::bail!(
            "codex session label '{}' has no entry in {} (session_index*.jsonl); \
             the /rename never registered or the index was pruned, so codex \
             cannot resume it. Respawn the agent to start a fresh session.",
            name,
            codex_home.display()
        ),
        [single] => {
            if uuid::Uuid::parse_str(&single.id).is_err() {
                anyhow::bail!(
                    "codex session label '{}' resolves to id '{}' in {}, which is not a UUID; \
                     refusing to resume by label",
                    name,
                    single.id,
                    codex_home.display()
                );
            }
            Ok(single.id.clone())
        }
        many => {
            let listed: Vec<String> = many
                .iter()
                .map(|c| match &c.updated_at {
                    Some(at) => format!("{} (updated {})", c.id, at),
                    None => c.id.clone(),
                })
                .collect();
            anyhow::bail!(
                "codex session label '{}' is ambiguous in {}: {} sessions carry it [{}]; \
                 stamp the intended UUID in <state_dir>/session-id or respawn the agent",
                name,
                codex_home.display(),
                many.len(),
                listed.join(", ")
            )
        }
    }
}

/// Paste `text` into a tmux target as a single block followed by Enter,
/// using load-buffer + paste-buffer (bracket paste mode) so newlines and
/// shell metacharacters survive.
fn paste_text(target: &str, text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

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

/// Ensure `[projects."<cwd>"] trust_level = "trusted"` is present in the
/// codex config TOML at `trust_file` (typically `~/.codex/config.toml`).
/// Tests pass an explicit `TempDir`-backed path; production calls into
/// `harness_home(Codex).join("config.toml")` which honors `AGMAN_CODEX_HOME`.
///
/// Behavior:
/// - File doesn't exist → create one with just the trust entry.
/// - File exists, no `[projects."<cwd>"]` table → add it.
/// - Table exists but no `trust_level` → set it to `"trusted"`.
/// - `trust_level` already `"trusted"` → no-op (file untouched).
/// - `trust_level` is `"untrusted"` → upgrade to `"trusted"`.
///
/// Other keys (root-level + other project tables) are preserved. Layout /
/// comments may be rewritten by `toml::to_string` — acceptable here because
/// the codex config is mostly machine-generated.
pub fn ensure_workspace_trusted_in(trust_file: &Path, cwd: &Path) -> Result<()> {
    use anyhow::Context;

    let cwd_str = cwd.to_string_lossy().to_string();

    // Parse the existing TOML or start from an empty table.
    let mut doc: toml::Value = if trust_file.exists() {
        let text = std::fs::read_to_string(trust_file)
            .with_context(|| format!("read codex trust file at {}", trust_file.display()))?;
        if text.trim().is_empty() {
            toml::Value::Table(toml::value::Table::new())
        } else {
            toml::from_str(&text).with_context(|| {
                format!("parse codex trust file at {} as TOML", trust_file.display())
            })?
        }
    } else {
        toml::Value::Table(toml::value::Table::new())
    };

    let root = doc.as_table_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "codex trust file at {} is not a TOML table",
            trust_file.display()
        )
    })?;

    // Idempotent fast-path: peek before mutating so we can skip the write
    // when the entry is already trusted (preserves mtime).
    let already_trusted = root
        .get("projects")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get(&cwd_str))
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("trust_level"))
        .and_then(|v| v.as_str())
        == Some("trusted");
    if already_trusted {
        return Ok(());
    }

    // Walk into projects.<cwd>.trust_level and set it.
    if !root.contains_key("projects") {
        root.insert(
            "projects".to_string(),
            toml::Value::Table(toml::value::Table::new()),
        );
    }
    let projects = root
        .get_mut("projects")
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "codex trust file at {} has non-table `projects`",
                trust_file.display()
            )
        })?;

    if !projects.contains_key(&cwd_str) {
        projects.insert(
            cwd_str.clone(),
            toml::Value::Table(toml::value::Table::new()),
        );
    }
    let project = projects
        .get_mut(&cwd_str)
        .and_then(|v| v.as_table_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "codex trust file at {} has non-table `projects.\"{}\"`",
                trust_file.display(),
                cwd_str
            )
        })?;

    project.insert(
        "trust_level".to_string(),
        toml::Value::String("trusted".to_string()),
    );

    write_atomically(trust_file, toml::to_string(&doc)?.as_bytes())
}

/// Ensure the Playwright MCP server is defined in codex config but disabled
/// by default. Tester launches opt in per process with a `-c` override.
pub fn ensure_browser_mcp_in(config_toml_path: &Path) -> Result<()> {
    use anyhow::Context;

    let mut doc: toml::Value = if config_toml_path.exists() {
        let text = std::fs::read_to_string(config_toml_path)
            .with_context(|| format!("read codex config file at {}", config_toml_path.display()))?;
        if text.trim().is_empty() {
            toml::Value::Table(toml::value::Table::new())
        } else {
            toml::from_str(&text).with_context(|| {
                format!(
                    "parse codex config file at {} as TOML",
                    config_toml_path.display()
                )
            })?
        }
    } else {
        toml::Value::Table(toml::value::Table::new())
    };

    let root = doc.as_table_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "codex config file at {} is not a TOML table",
            config_toml_path.display()
        )
    })?;

    let mcp_servers = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "codex config file at {} has non-table `mcp_servers`",
                config_toml_path.display()
            )
        })?;

    if mcp_servers.contains_key("playwright") {
        return Ok(());
    }

    let mut playwright = toml::value::Table::new();
    playwright.insert(
        "command".to_string(),
        toml::Value::String("npx".to_string()),
    );
    playwright.insert(
        "args".to_string(),
        toml::Value::Array(vec![toml::Value::String(
            "@playwright/mcp@latest".to_string(),
        )]),
    );
    playwright.insert(
        "env_vars".to_string(),
        toml::Value::Array(
            [
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "XAUTHORITY",
                "XDG_RUNTIME_DIR",
            ]
            .into_iter()
            .map(|v| toml::Value::String(v.to_string()))
            .collect(),
        ),
    );
    playwright.insert("enabled".to_string(), toml::Value::Boolean(false));
    mcp_servers.insert("playwright".to_string(), toml::Value::Table(playwright));

    write_atomically(config_toml_path, toml::to_string(&doc)?.as_bytes())
}

/// Write `bytes` to `dest` atomically: write to `<dest>.tmp`, fsync, rename.
/// Creates `dest`'s parent directory if missing.
fn write_atomically(dest: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_path = dest.as_os_str().to_owned();
    tmp_path.push(".tmp");
    let tmp_path = PathBuf::from(tmp_path);
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, dest)?;
    Ok(())
}
