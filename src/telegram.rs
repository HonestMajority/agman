//! Minimal Telegram bot integration for the Chief of Staff agent.
//!
//! When `telegram_bot_token` and `telegram_chat_id` are configured in the
//! settings UI (stored in `config.toml`), a background thread bridges
//! plain-text messages between the user's Telegram chat and the Chief of
//! Staff tmux session via the existing inbox/outbox JSONL files.
//!
//! Voice messages are transcribed locally via `whisper-cli` and forwarded as
//! text. The whisper GGML model is auto-downloaded on first use.
//!
//! If not configured, the feature is completely dormant.

use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::assistant::{Assistant, AssistantKind, AssistantStatus};
use crate::config::Config;
use crate::inbox;
use crate::use_cases;

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Failure modes for Telegram API calls.
///
/// - `Permanent` means the message will never succeed (HTTP 4xx — bad request,
///   bot kicked from chat, etc.) and the caller should abandon it.
/// - `Transient` means it might succeed on retry (HTTP 5xx, network, JSON
///   parse) and the caller should stop and try again next cycle.
#[derive(Debug)]
pub enum TgError {
    Permanent,
    Transient,
}

/// What `drain_outbox` should do with a given message based on the send result.
#[derive(Debug, PartialEq, Eq)]
pub enum OutboxAction {
    MarkDelivered,
    DeadLetter,
    Stop,
}

/// Pure classifier — extracted so it can be unit-tested without a network call.
pub fn classify_outbox_result(result: Result<(), TgError>) -> OutboxAction {
    match result {
        Ok(()) => OutboxAction::MarkDelivered,
        Err(TgError::Permanent) => OutboxAction::DeadLetter,
        Err(TgError::Transient) => OutboxAction::Stop,
    }
}

// ---------------------------------------------------------------------------
// Panic recovery helpers
// ---------------------------------------------------------------------------

/// Standard `Box<dyn Any>` panic-payload downcast: try `&str`, then `String`,
/// else fall back to a sentinel.
fn extract_panic_msg(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic>".to_string()
    }
}

/// Append `<rfc3339 ts>\t<msg>\n` to the never-rotated panic log so evidence
/// survives rotation of the main `agman.log`. Best-effort — IO errors are
/// swallowed (we must never panic-on-panic).
fn append_panic_log(path: &Path, panic_msg: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}\t{}", chrono::Utc::now().to_rfc3339(), panic_msg);
    }
}

/// Run a closure under `catch_unwind`, returning `Err(extracted_msg)` on panic.
///
/// Exposed so the production loop and tests share the same panic-handling
/// code path.
pub fn run_iter_catching_panic<F: FnOnce() + std::panic::UnwindSafe>(f: F) -> Result<(), String> {
    catch_unwind(f).map_err(|payload| extract_panic_msg(&*payload))
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct BotCtx {
    agent: ureq::Agent,
    base: String,
    file_base: String,
    chat_id: String,
    whisper_model: String,
    config: Config,
    current_agent: Arc<RwLock<String>>,
    telegram_outbox: PathBuf,
    telegram_outbox_seq: PathBuf,
    telegram_dead_letter: PathBuf,
}

/// Filesystem paths the bot needs. Bundled to keep `run_bot`'s arg count sane.
struct BotPaths {
    telegram_outbox: PathBuf,
    telegram_outbox_seq: PathBuf,
    telegram_dead_letter: PathBuf,
}

struct BotRunArgs {
    token: String,
    chat_id: String,
    cancel: Arc<AtomicBool>,
    paths: BotPaths,
    whisper_model: String,
    heartbeat: Arc<AtomicU64>,
    panic_log_path: PathBuf,
    config: Config,
    current_agent: Arc<RwLock<String>>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Handle returned by [`start`]: shutdown flag + heartbeat (epoch seconds, 0 = never)
/// + thread join handle (held for potential future clean shutdown via `.join()`).
pub struct TelegramHandle {
    pub cancel: Arc<AtomicBool>,
    pub heartbeat: Arc<AtomicU64>,
    pub join: std::thread::JoinHandle<()>,
}

/// Spawn the Telegram bot in a self-healing background thread.
///
/// Returns `None` when token/chat_id are empty so callers don't pre-build state
/// for a doomed thread. The thread:
/// - Catches per-iteration panics (`drain_outbox` / `poll_updates`), logs them,
///   sleeps 5s, and continues.
/// - Catches setup panics (e.g. during `BotCtx` construction), logs them,
///   sleeps 10s, and retries forever — the thread only exits cleanly when
///   `cancel` is set. Combined with the TUI watchdog (which sets `cancel` and
///   respawns on stalls), this closes the "thread gone forever" gap.
/// - Bumps the `heartbeat` atomic each loop iteration so the TUI can show
///   liveness independent of network success.
pub fn start(config: &Config, token: String, chat_id: String) -> Option<TelegramHandle> {
    if token.trim().is_empty() || chat_id.trim().is_empty() {
        return None;
    }

    let paths = BotPaths {
        telegram_outbox: config.telegram_outbox(),
        telegram_outbox_seq: config.telegram_outbox_seq(),
        telegram_dead_letter: config.telegram_dead_letter(),
    };
    let whisper_model = std::env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| config.whisper_model_path().to_string_lossy().into_owned());
    let panic_log_path = config.telegram_panic_log();
    let initial_agent = use_cases::read_current_agent(config);
    let current_agent = Arc::new(RwLock::new(initial_agent));
    let config_owned = config.clone();

    let cancel = Arc::new(AtomicBool::new(false));
    let heartbeat = Arc::new(AtomicU64::new(0));

    let cancel_thread = Arc::clone(&cancel);
    let heartbeat_thread = Arc::clone(&heartbeat);

    let join = std::thread::spawn(move || {
        let mut attempt: u32 = 0;
        loop {
            if cancel_thread.load(Ordering::Relaxed) {
                break;
            }
            attempt = attempt.saturating_add(1);
            let token = token.clone();
            let chat_id = chat_id.clone();
            let whisper_model = whisper_model.clone();
            let cancel = Arc::clone(&cancel_thread);
            let heartbeat = Arc::clone(&heartbeat_thread);
            let panic_log = panic_log_path.clone();
            let config = config_owned.clone();
            let current_agent = Arc::clone(&current_agent);
            let paths = BotPaths {
                telegram_outbox: paths.telegram_outbox.clone(),
                telegram_outbox_seq: paths.telegram_outbox_seq.clone(),
                telegram_dead_letter: paths.telegram_dead_letter.clone(),
            };

            let result = catch_unwind(AssertUnwindSafe(move || {
                run_bot(BotRunArgs {
                    token,
                    chat_id,
                    cancel,
                    paths,
                    whisper_model,
                    heartbeat,
                    panic_log_path: panic_log,
                    config,
                    current_agent,
                });
            }));

            match result {
                Ok(()) => return,
                Err(payload) => {
                    let msg = extract_panic_msg(&*payload);
                    tracing::warn!(
                        attempt = attempt,
                        panic_msg = %msg,
                        "telegram bot: setup panic, retrying",
                    );
                    append_panic_log(
                        &panic_log_path,
                        &format!("setup panic (attempt {attempt}): {msg}"),
                    );
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
        }
    });

    Some(TelegramHandle {
        cancel,
        heartbeat,
        join,
    })
}

// ---------------------------------------------------------------------------
// Bot loop
// ---------------------------------------------------------------------------

fn run_bot(args: BotRunArgs) {
    let BotRunArgs {
        token,
        chat_id,
        cancel,
        paths,
        whisper_model,
        heartbeat,
        panic_log_path,
        config,
        current_agent,
    } = args;

    if token.trim().is_empty() || chat_id.trim().is_empty() {
        tracing::warn!("telegram bot not started: token or chat_id is empty");
        return;
    }

    tracing::info!(chat_id = %chat_id, token_len = token.len(), "telegram bot starting");

    let ctx = BotCtx {
        agent: ureq::AgentBuilder::new()
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .build(),
        file_base: format!("https://api.telegram.org/file/bot{token}"),
        base: format!("https://api.telegram.org/bot{token}"),
        chat_id,
        whisper_model,
        config,
        current_agent,
        telegram_outbox: paths.telegram_outbox,
        telegram_outbox_seq: paths.telegram_outbox_seq,
        telegram_dead_letter: paths.telegram_dead_letter,
    };

    register_bot_commands(&ctx);

    let mut offset: i64 = 0;

    while !cancel.load(Ordering::Relaxed) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        heartbeat.store(now, Ordering::Relaxed);

        let result = run_iter_catching_panic(AssertUnwindSafe(|| {
            drain_outbox(&ctx);
            poll_updates(&ctx, &mut offset);
        }));
        match result {
            Ok(()) => {
                tracing::debug!("telegram bot: poll cycle complete");
            }
            Err(msg) => {
                tracing::error!(
                    panic_msg = %msg,
                    chat_id_len = ctx.chat_id.len(),
                    "telegram bot: iteration panic, recovering",
                );
                append_panic_log(&panic_log_path, &format!("iteration panic: {msg}"));
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }

    tracing::info!("telegram bot shutting down");
}

// ---------------------------------------------------------------------------
// Inbound: Telegram -> Chief of Staff inbox
// ---------------------------------------------------------------------------

fn poll_updates(ctx: &BotCtx, offset: &mut i64) {
    let body = serde_json::json!({
        "offset": *offset,
        "timeout": 2,
        "allowed_updates": ["message", "callback_query"],
    });

    let resp = match tg_post(&ctx.agent, &format!("{}/getUpdates", ctx.base), &body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(updates) = resp["result"].as_array() else {
        return;
    };

    for update in updates {
        if let Some(uid) = update["update_id"].as_i64() {
            *offset = uid + 1;
        }

        if let Some(cq) = update.get("callback_query") {
            handle_callback_query(ctx, cq);
            continue;
        }

        let Some(msg) = update.get("message") else {
            continue;
        };
        let Some(msg_chat_id) = msg["chat"]["id"].as_i64() else {
            continue;
        };
        if msg_chat_id.to_string() != ctx.chat_id {
            tracing::warn!(msg_chat_id = msg_chat_id, expected = %ctx.chat_id, "telegram: rejected message from unauthorized chat_id");
            continue;
        }

        // Handle voice messages.
        if msg.get("voice").is_some() {
            if let Some(file_id) = msg["voice"]["file_id"].as_str() {
                let reply_text = msg["reply_to_message"]["text"].as_str();
                handle_voice(ctx, file_id, reply_text);
            }
            continue;
        }

        // Handle text messages.
        let Some(text) = msg["text"].as_str() else {
            continue;
        };

        let trimmed = text.trim();
        match trimmed {
            "/ls" => {
                tracing::info!("telegram: /ls command");
                let (reply_text, buttons) = build_ls_reply(ctx);
                let _ = tg_send_with_keyboard(ctx, &reply_text, buttons);
                continue;
            }
            "/who" => {
                tracing::info!("telegram: /who command");
                let current = ctx
                    .current_agent
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                let _ = tg_send(ctx, &format!("📍 Talking to: {current}"));
                continue;
            }
            "/back" => {
                tracing::info!("telegram: /back command");
                let current = ctx
                    .current_agent
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                match parent_of(&current) {
                    Some(parent) => switch_current_agent(ctx, &parent),
                    None => {
                        let _ = tg_send(ctx, "📍 Already at Chief of Staff (root).");
                    }
                }
                continue;
            }
            _ => {}
        }

        let reply_text = msg["reply_to_message"]["text"].as_str();
        let (inbox_path, agent_label, payload) = route_inbound_message(ctx, reply_text, text);
        tracing::info!(text_len = text.len(), agent = %agent_label, "telegram: received message");
        if let Err(e) = inbox::append_message(&inbox_path, "telegram", &payload) {
            tracing::warn!(error = %e, agent = %agent_label, "telegram: failed to write to agent inbox");
        }
    }
}

/// Extract the leading `[TAG]` from a bot-formatted message.
///
/// Returns the inner tag (without brackets) when `text` starts with `[` and
/// has a closing `]` later in the string. Returns `None` for plain text or
/// unbalanced brackets.
pub fn parse_sender_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('[')?;
    let close = rest.find(']')?;
    Some(&rest[..close])
}

/// Resolve a sender tag (e.g. `"CoS"`, `"PM:foo"`, `"R:bar"`) back to a
/// send-target agent id usable with [`use_cases::agent_inbox_path`].
///
/// - `"CoS"` → `Some("chief-of-staff")`
/// - `"PM:<id>"` → `Some(id)` if the project agent exists
/// - `"R:<name>"` → `Some("researcher:<project>--<name>")` when exactly one
///   running researcher matches; `None` for zero or multiple matches.
/// - `"Rv:<name>"` → `Some("reviewer:<project>--<name>")` with the same
///   uniqueness rules.
///
/// Ambiguous matches are logged via `tracing::warn`.
///
/// Defensively returns `None` if the resolved id is `"telegram"` to prevent
/// the bot from looping messages back to itself.
pub fn resolve_tag_to_agent(config: &Config, tag: &str) -> Option<String> {
    let resolved = if tag == "CoS" {
        Some("chief-of-staff".to_string())
    } else if let Some(id) = tag.strip_prefix("PM:") {
        if !id.is_empty() && use_cases::agent_exists(config, id) {
            Some(id.to_string())
        } else {
            None
        }
    } else if let Some(name) = tag.strip_prefix("Rv:") {
        resolve_assistant_tag(config, name, AssistantKindFilter::Reviewer)
    } else if let Some(name) = tag.strip_prefix("R:") {
        resolve_assistant_tag(config, name, AssistantKindFilter::Researcher)
    } else {
        None
    }?;

    if resolved == "telegram" {
        return None;
    }
    Some(resolved)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantKindFilter {
    Researcher,
    Reviewer,
}

fn resolve_assistant_tag(config: &Config, name: &str, kind: AssistantKindFilter) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let assistants = match Assistant::list_all(config) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "telegram: failed to list assistants for tag resolution");
            return None;
        }
    };
    let matches: Vec<&Assistant> = assistants
        .iter()
        .filter(|a| a.meta.name == name && a.meta.status == AssistantStatus::Running)
        .filter(|a| {
            matches!(
                (&a.meta.kind, kind),
                (
                    AssistantKind::Researcher { .. },
                    AssistantKindFilter::Researcher
                ) | (
                    AssistantKind::Reviewer { .. },
                    AssistantKindFilter::Reviewer
                )
            )
        })
        .collect();
    match matches.len() {
        1 => {
            let a = matches[0];
            let prefix = match kind {
                AssistantKindFilter::Researcher => "researcher",
                AssistantKindFilter::Reviewer => "reviewer",
            };
            Some(format!("{prefix}:{}--{}", a.meta.project, a.meta.name))
        }
        0 => None,
        n => {
            tracing::warn!(
                tag = %name,
                count = n,
                kind = ?kind,
                "telegram: ambiguous assistant tag, falling back"
            );
            None
        }
    }
}

/// Build the inbox payload for a reply: a one-line context snippet of the
/// original bot message followed by a blank line and the user's body.
///
/// The leading `[TAG]` is stripped from the original (if present) and the
/// snippet is truncated to ~140 chars (UTF-8 safe), with an ellipsis appended
/// when the snippet was cut short.
pub fn format_reply_message(original: &str, body: &str) -> String {
    let stripped = if let Some(tag) = parse_sender_tag(original) {
        // Skip past `[TAG]` — `[` + tag bytes + `]`.
        &original[1 + tag.len() + 1..]
    } else {
        original
    };
    let stripped = stripped.trim_start();

    const SNIPPET_CHARS: usize = 140;
    let snippet: String = stripped.chars().take(SNIPPET_CHARS).collect();
    let snippet = if stripped.chars().count() > SNIPPET_CHARS {
        format!("{snippet}…")
    } else {
        snippet
    };

    format!("In reply to: \"{snippet}\"\n\n{body}")
}

/// Snapshot the current agent id from the shared `RwLock`, recovering from
/// poison by cloning the inner value.
fn read_current_agent(ctx: &BotCtx) -> String {
    ctx.current_agent
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone())
}

/// Decide where an inbound user message should be delivered.
///
/// If the message is a reply to a bot-tagged message that resolves to a known
/// agent, route to that agent's inbox with reply context. Otherwise fall back
/// to the user's current agent. Routing is one-shot — the current-agent
/// setting is *not* mutated.
///
/// Returns `(inbox_path, agent_id_for_logging, payload_to_write)`.
fn route_inbound_message(
    ctx: &BotCtx,
    reply_text: Option<&str>,
    body: &str,
) -> (PathBuf, String, String) {
    let routed = reply_text
        .and_then(parse_sender_tag)
        .and_then(|tag| resolve_tag_to_agent(&ctx.config, tag));

    if let Some(target) = routed {
        match use_cases::agent_inbox_path(&ctx.config, &target) {
            Ok(path) => {
                let original = reply_text.unwrap_or("");
                let payload = format_reply_message(original, body);
                tracing::info!(
                    target = %target,
                    "telegram: routing reply to tagged agent"
                );
                return (path, target, payload);
            }
            Err(e) => {
                tracing::warn!(
                    target = %target,
                    error = %e,
                    "telegram: routed agent inbox lookup failed, falling back to current agent"
                );
            }
        }
    }

    (
        current_inbox(ctx),
        read_current_agent(ctx),
        body.to_string(),
    )
}

fn handle_callback_query(ctx: &BotCtx, cq: &Value) {
    let callback_id = cq["id"].as_str().unwrap_or_default();

    let msg_chat_id = cq["message"]["chat"]["id"].as_i64();
    match msg_chat_id {
        Some(id) if id.to_string() == ctx.chat_id => {}
        _ => {
            tracing::warn!(
                got = ?msg_chat_id,
                expected = %ctx.chat_id,
                "telegram: rejected callback_query from unauthorized chat"
            );
            return;
        }
    }

    if let Some(data) = cq["data"].as_str() {
        if let Some(target) = data.strip_prefix("switch:") {
            switch_current_agent(ctx, target);
        } else {
            tracing::warn!(data = %data, "telegram: unknown callback data");
        }
    }

    if !callback_id.is_empty() {
        tg_answer_callback(ctx, callback_id);
    }
}

// ---------------------------------------------------------------------------
// Outbound: Chief of Staff outbox -> Telegram
// ---------------------------------------------------------------------------

fn drain_outbox(ctx: &BotCtx) {
    let messages = match inbox::read_undelivered(&ctx.telegram_outbox, &ctx.telegram_outbox_seq) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::debug!(error = %e, "telegram: failed to read outbox");
            return;
        }
    };

    for msg in messages {
        let tagged = format!("[{}] {}", format_sender_tag(&msg.from), msg.message);
        let result = tg_send(ctx, &tagged);
        match classify_outbox_result(result) {
            OutboxAction::MarkDelivered => {
                if let Err(e) = inbox::mark_delivered(&ctx.telegram_outbox_seq, msg.seq) {
                    tracing::warn!(error = %e, seq = msg.seq, "telegram: failed to mark delivered");
                }
            }
            OutboxAction::DeadLetter => {
                let preview: String = msg.message.chars().take(80).collect();
                tracing::error!(
                    seq = msg.seq,
                    from = %msg.from,
                    preview = %preview,
                    "telegram: permanent send failure, moving to dead-letter queue",
                );
                if let Err(e) =
                    append_dead_letter(&ctx.telegram_dead_letter, &msg, "permanent send failure")
                {
                    tracing::warn!(error = %e, seq = msg.seq, "telegram: failed to write dead-letter entry");
                }
                // Mark delivered anyway so the queue unblocks for subsequent messages.
                if let Err(e) = inbox::mark_delivered(&ctx.telegram_outbox_seq, msg.seq) {
                    tracing::warn!(error = %e, seq = msg.seq, "telegram: failed to mark dead-lettered message delivered");
                }
            }
            OutboxAction::Stop => {
                // Transient error — stop and retry next cycle.
                break;
            }
        }
    }
}

/// Append a failed outbox message to the dead-letter JSONL file.
fn append_dead_letter(path: &Path, msg: &inbox::InboxMessage, reason: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entry = serde_json::json!({
        "seq": msg.seq,
        "from": msg.from,
        "message": msg.message,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "reason": reason,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", entry)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Telegram API helpers
// ---------------------------------------------------------------------------

/// Short human-readable sender tag rendered on outbound Telegram messages.
///
/// - `"chief-of-staff"` → `"CoS"`
/// - `"researcher:<project>--<name>"` → `"R:<name>"` (text after the last `--`)
/// - `"reviewer:<project>--<name>"` → `"Rv:<name>"` (text after the last `--`)
/// - anything else → `"PM:<from>"` (default — project names live here)
pub fn format_sender_tag(from: &str) -> String {
    if from == "chief-of-staff" {
        return "CoS".to_string();
    }
    if let Some(rest) = from.strip_prefix("researcher:") {
        let name = rest.rsplit("--").next().unwrap_or(rest);
        return format!("R:{name}");
    }
    if let Some(rest) = from.strip_prefix("reviewer:") {
        let name = rest.rsplit("--").next().unwrap_or(rest);
        return format!("Rv:{name}");
    }
    format!("PM:{from}")
}

fn tg_post(agent: &ureq::Agent, url: &str, body: &Value) -> Result<Value, TgError> {
    let method = url.rsplit('/').next().unwrap_or("?");
    tracing::debug!(method = method, "telegram: API request");

    match agent.post(url).send_json(body) {
        Ok(resp) => match resp.into_json::<Value>() {
            Ok(val) => Ok(val),
            Err(e) => {
                tracing::warn!(method = method, error = %e, "telegram: response parse error");
                Err(TgError::Transient)
            }
        },
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            tracing::warn!(
                method = method,
                status = code,
                body = body,
                "telegram: API error"
            );
            if (400..=499).contains(&code) {
                Err(TgError::Permanent)
            } else {
                Err(TgError::Transient)
            }
        }
        Err(e) => {
            tracing::warn!(method = method, error = %e, "telegram: transport error");
            Err(TgError::Transient)
        }
    }
}

/// Send a plain-text message. Returns `Ok(())` on success, classified error otherwise.
fn tg_send(ctx: &BotCtx, text: &str) -> Result<(), TgError> {
    let body = serde_json::json!({
        "chat_id": ctx.chat_id,
        "text": text,
    });
    match tg_post(&ctx.agent, &format!("{}/sendMessage", ctx.base), &body) {
        Ok(_) => {
            tracing::info!(text_len = text.len(), "telegram: sent message");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Register the bot's supported slash commands with Telegram so they appear in
/// the command menu on the client side. Best-effort — logs on failure.
fn register_bot_commands(ctx: &BotCtx) {
    let body = serde_json::json!({
        "commands": [
            {"command": "ls", "description": "List agents you can switch to"},
            {"command": "back", "description": "Switch to parent agent"},
            {"command": "who", "description": "Show current agent"},
        ]
    });
    if let Err(e) = tg_post(&ctx.agent, &format!("{}/setMyCommands", ctx.base), &body) {
        tracing::warn!(error = ?e, "telegram: setMyCommands failed");
    }
}

/// Send a message with an inline keyboard. `buttons` is a grid — outer vec is
/// rows, inner tuples are `(label, callback_data)`.
fn tg_send_with_keyboard(
    ctx: &BotCtx,
    text: &str,
    buttons: Vec<Vec<(String, String)>>,
) -> Result<(), TgError> {
    let keyboard: Vec<Vec<Value>> = buttons
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(label, mut data)| {
                    if data.len() > 64 {
                        tracing::warn!(
                            data = %data,
                            "telegram: callback_data exceeded 64 bytes, truncating"
                        );
                        data.truncate(64);
                    }
                    serde_json::json!({"text": label, "callback_data": data})
                })
                .collect()
        })
        .collect();

    let body = serde_json::json!({
        "chat_id": ctx.chat_id,
        "text": text,
        "reply_markup": {"inline_keyboard": keyboard},
    });
    match tg_post(&ctx.agent, &format!("{}/sendMessage", ctx.base), &body) {
        Ok(_) => {
            tracing::info!(text_len = text.len(), "telegram: sent keyboard message");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Dismiss the loading spinner on a tapped inline-keyboard button. Best-effort.
fn tg_answer_callback(ctx: &BotCtx, callback_id: &str) {
    let body = serde_json::json!({"callback_query_id": callback_id});
    if let Err(e) = tg_post(
        &ctx.agent,
        &format!("{}/answerCallbackQuery", ctx.base),
        &body,
    ) {
        tracing::debug!(error = ?e, "telegram: answerCallbackQuery failed");
    }
}

/// Resolve the inbox for the currently-selected agent. On a stale agent
/// (resolution fails) we reset both in-memory and on-disk state back to
/// `"chief-of-staff"`, notify the user, and return the Chief of Staff inbox.
fn current_inbox(ctx: &BotCtx) -> PathBuf {
    let id = ctx
        .current_agent
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    match use_cases::agent_inbox_path(&ctx.config, &id) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!(
                old = %id,
                error = %e,
                "telegram: current agent is stale, resetting to chief-of-staff"
            );
            match ctx.current_agent.write() {
                Ok(mut g) => *g = "chief-of-staff".to_string(),
                Err(p) => {
                    let mut g = p.into_inner();
                    *g = "chief-of-staff".to_string();
                }
            }
            if let Err(e) = use_cases::write_current_agent(&ctx.config, "chief-of-staff") {
                tracing::warn!(error = %e, "telegram: failed to persist reset to chief-of-staff");
            }
            let _ = tg_send(
                ctx,
                "⚠️ The agent you were talking to no longer exists. Reset to Chief of Staff.",
            );
            ctx.config.chief_of_staff_inbox()
        }
    }
}

/// Build the `/ls` reply: header showing the current agent + inline-keyboard
/// buttons for each reachable agent, two per row.
fn build_ls_reply(ctx: &BotCtx) -> (String, Vec<Vec<(String, String)>>) {
    let current = ctx
        .current_agent
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    let agents = use_cases::relative_agent_list(&ctx.config, &current);

    if agents.is_empty() {
        return (
            format!("📍 Current: {current}\n\n(no other agents available)"),
            vec![],
        );
    }

    let buttons: Vec<Vec<(String, String)>> = agents
        .chunks(2)
        .map(|chunk| {
            chunk
                .iter()
                .map(|a| (a.label.clone(), format!("switch:{}", a.id)))
                .collect()
        })
        .collect();

    (format!("📍 Current: {current}\n\nSwitch to:"), buttons)
}

/// Parent agent for `/back`. Returns `None` when already at root
/// (`"chief-of-staff"`).
pub fn parent_of(current: &str) -> Option<String> {
    if current == "chief-of-staff" {
        return None;
    }
    for prefix in ["researcher:", "reviewer:"] {
        if let Some(rest) = current.strip_prefix(prefix) {
            if let Some(pos) = rest.find("--") {
                let project = &rest[..pos];
                return Some(project.to_string());
            }
        }
    }
    Some("chief-of-staff".to_string())
}

/// Validate and apply a current-agent switch. Persists, updates in-memory
/// state, notifies the user, and logs old→new.
fn switch_current_agent(ctx: &BotCtx, target: &str) {
    if !use_cases::agent_exists(&ctx.config, target) {
        tracing::warn!(target = %target, "telegram: rejected switch to unknown agent");
        let _ = tg_send(ctx, &format!("⚠️ Unknown agent: {target}"));
        return;
    }

    let old = ctx
        .current_agent
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|p| p.into_inner().clone());

    if let Err(e) = use_cases::write_current_agent(&ctx.config, target) {
        tracing::warn!(error = %e, target = %target, "telegram: failed to persist current agent");
    }

    match ctx.current_agent.write() {
        Ok(mut g) => *g = target.to_string(),
        Err(p) => {
            let mut g = p.into_inner();
            *g = target.to_string();
        }
    }

    tracing::info!(old = %old, new = %target, "telegram: switched current agent");
    let _ = tg_send(ctx, &format!("📍 Now talking to: {target}"));
}

// ---------------------------------------------------------------------------
// Voice message handling
// ---------------------------------------------------------------------------

/// Check whether `ffmpeg` and `whisper-cli` are available on PATH.
///
/// Returns `Some("binary_name")` for the first missing binary, or `None` if
/// both are present.
fn check_voice_deps() -> Option<String> {
    use std::process::Command;

    for bin in &["ffmpeg", "whisper-cli"] {
        match Command::new(bin).arg("-version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Some(bin.to_string());
            }
            _ => {}
        }
    }
    None
}

/// Process an incoming voice message: download, transcribe locally, forward
/// to the routed agent (when the voice was sent as a reply to a tagged bot
/// message) or the user's current agent.
fn handle_voice(ctx: &BotCtx, file_id: &str, reply_text: Option<&str>) {
    if let Some(binary) = check_voice_deps() {
        tracing::warn!(binary = %binary, "telegram: voice dependency not found");
        let _ = tg_send(
            ctx,
            &format!("⚠️ Voice transcription requires `{binary}` but it's not installed. Please install it and try again."),
        );
        return;
    }

    let Some(audio_data) = download_voice(&ctx.agent, &ctx.base, &ctx.file_base, file_id) else {
        let _ = tg_send(ctx, "Failed to download voice message.");
        return;
    };

    if !ensure_whisper_model(ctx) {
        return;
    }

    match transcribe_audio(&ctx.whisper_model, &audio_data) {
        Some(text) if !text.is_empty() => {
            tracing::info!(text_len = text.len(), "telegram: transcribed voice message");
            let _ = tg_send(ctx, &format!("🎤 {text}"));
            let (inbox_path, agent_label, payload) = route_inbound_message(ctx, reply_text, &text);
            if let Err(e) = inbox::append_message(&inbox_path, "telegram", &payload) {
                tracing::warn!(
                    error = %e,
                    agent = %agent_label,
                    "telegram: failed to write transcription to agent inbox"
                );
            }
        }
        _ => {
            let _ = tg_send(ctx, "Failed to transcribe voice message.");
        }
    }
}

/// Download a voice file from Telegram using the Bot File API.
///
/// 1. Call `getFile` to resolve the `file_id` to a `file_path`.
/// 2. GET the file bytes from `https://api.telegram.org/file/bot<token>/<file_path>`.
fn download_voice(
    agent: &ureq::Agent,
    base: &str,
    file_base: &str,
    file_id: &str,
) -> Option<Vec<u8>> {
    let body = serde_json::json!({"file_id": file_id});
    let resp = tg_post(agent, &format!("{base}/getFile"), &body).ok()?;
    let file_path = resp["result"]["file_path"].as_str()?;

    let url = format!("{file_base}/{file_path}");
    tracing::debug!(url = %url, "telegram: downloading voice file");
    match agent.get(&url).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            if resp.into_reader().read_to_end(&mut buf).is_ok() {
                tracing::debug!(bytes = buf.len(), "telegram: downloaded voice file");
                Some(buf)
            } else {
                tracing::warn!("telegram: failed to read voice response body");
                None
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "telegram: voice download error");
            None
        }
    }
}

/// Ensure the whisper GGML model file exists, downloading it if necessary.
///
/// Uses the HuggingFace-hosted ggml-base.bin (~142 MB) and streams it directly
/// to disk so we don't buffer the whole file in memory.
fn ensure_whisper_model(ctx: &BotCtx) -> bool {
    use std::path::Path;

    if Path::new(&ctx.whisper_model).exists() {
        return true;
    }

    tracing::info!(path = %ctx.whisper_model, "telegram: whisper model not found, downloading");
    let _ = tg_send(ctx, "Downloading whisper model (first time only)...");

    let url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";
    let dl_agent = ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(300))
        .build();

    match dl_agent.get(url).call() {
        Ok(resp) => {
            if let Some(parent) = Path::new(&ctx.whisper_model).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut file = match std::fs::File::create(&ctx.whisper_model) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(error = %e, "telegram: failed to create model file");
                    let _ = tg_send(ctx, "Failed to save whisper model.");
                    return false;
                }
            };
            if let Err(e) = std::io::copy(&mut resp.into_reader(), &mut file) {
                tracing::error!(error = %e, "telegram: failed to write model file");
                let _ = std::fs::remove_file(&ctx.whisper_model);
                let _ = tg_send(ctx, "Failed to download whisper model.");
                return false;
            }
            tracing::info!("telegram: whisper model downloaded successfully");
            true
        }
        Err(e) => {
            tracing::error!(error = %e, "telegram: model download error");
            let _ = tg_send(ctx, "Failed to download whisper model.");
            false
        }
    }
}

/// Transcribe audio using the local `whisper-cli` binary.
///
/// Telegram sends OGG/Opus which whisper-cli can't decode directly,
/// so we convert to WAV via ffmpeg first.
fn transcribe_audio(model_path: &str, audio_data: &[u8]) -> Option<String> {
    use std::process::Command;

    let pid = std::process::id();
    let ogg_tmp = std::env::temp_dir().join(format!("voice-{pid}.ogg"));
    let wav_tmp = std::env::temp_dir().join(format!("voice-{pid}.wav"));

    if std::fs::write(&ogg_tmp, audio_data).is_err() {
        tracing::warn!("telegram: failed to write temp audio file");
        return None;
    }

    // Convert OGG/Opus -> WAV (16kHz mono, whisper's expected format).
    tracing::debug!(
        bytes = audio_data.len(),
        "telegram: ffmpeg converting OGG to WAV"
    );
    let ffmpeg = Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(&ogg_tmp)
        .args(["-ar", "16000", "-ac", "1", "-f", "wav"])
        .arg(&wav_tmp)
        .output();

    let _ = std::fs::remove_file(&ogg_tmp);

    match &ffmpeg {
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::warn!(stderr = %stderr, "telegram: ffmpeg failed");
            return None;
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::warn!("telegram: ffmpeg not found");
            } else {
                tracing::warn!(error = %e, "telegram: ffmpeg spawn error");
            }
            return None;
        }
        _ => {}
    }

    tracing::debug!("telegram: running whisper-cli transcription");

    let result = Command::new("whisper-cli")
        .args([
            "-m", model_path, "-nt", // no timestamps
            "-np", // no extra prints
            "-l", "auto", // auto-detect language
        ])
        .arg(&wav_tmp)
        .output();

    let _ = std::fs::remove_file(&wav_tmp);

    match result {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            tracing::debug!(text = %text, "telegram: whisper-cli output");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(stderr = %stderr, "telegram: whisper-cli failed");
            None
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::warn!("telegram: whisper-cli not found");
            } else {
                tracing::warn!(error = %e, "telegram: whisper-cli spawn error");
            }
            None
        }
    }
}
