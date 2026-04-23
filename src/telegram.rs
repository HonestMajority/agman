//! Minimal Telegram bot integration for the CEO agent.
//!
//! When `telegram_bot_token` and `telegram_chat_id` are configured in the
//! settings UI (stored in `config.toml`), a background thread bridges
//! plain-text messages between the user's Telegram chat and the CEO tmux
//! session via the existing inbox/outbox JSONL files.
//!
//! Voice messages are transcribed locally via `whisper-cli` and forwarded as
//! text. The whisper GGML model is auto-downloaded on first use.
//!
//! If not configured, the feature is completely dormant.

use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::Config;
use crate::inbox;

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
    ceo_inbox: PathBuf,
    telegram_outbox: PathBuf,
    telegram_outbox_seq: PathBuf,
    telegram_dead_letter: PathBuf,
}

/// Filesystem paths the bot needs. Bundled to keep `run_bot`'s arg count sane.
struct BotPaths {
    ceo_inbox: PathBuf,
    telegram_outbox: PathBuf,
    telegram_outbox_seq: PathBuf,
    telegram_dead_letter: PathBuf,
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
/// - Catches setup panics (e.g. during `BotCtx` construction), retries up to 3
///   times with a 10s gap between attempts, then exits cleanly.
/// - Bumps the `heartbeat` atomic each loop iteration so the TUI can show
///   liveness independent of network success.
pub fn start(config: &Config, token: String, chat_id: String) -> Option<TelegramHandle> {
    if token.trim().is_empty() || chat_id.trim().is_empty() {
        return None;
    }

    let paths = BotPaths {
        ceo_inbox: config.ceo_inbox(),
        telegram_outbox: config.telegram_outbox(),
        telegram_outbox_seq: config.telegram_outbox_seq(),
        telegram_dead_letter: config.telegram_dead_letter(),
    };
    let whisper_model = std::env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| config.whisper_model_path().to_string_lossy().into_owned());
    let panic_log_path = config.telegram_panic_log();

    let cancel = Arc::new(AtomicBool::new(false));
    let heartbeat = Arc::new(AtomicU64::new(0));

    let cancel_thread = Arc::clone(&cancel);
    let heartbeat_thread = Arc::clone(&heartbeat);

    let join = std::thread::spawn(move || {
        for attempt in 1..=3u32 {
            if cancel_thread.load(Ordering::Relaxed) {
                break;
            }
            // Each retry attempt gets fresh clones; cancel/heartbeat are Arc clones.
            let token = token.clone();
            let chat_id = chat_id.clone();
            let whisper_model = whisper_model.clone();
            let cancel = Arc::clone(&cancel_thread);
            let heartbeat = Arc::clone(&heartbeat_thread);
            let panic_log = panic_log_path.clone();
            let paths = BotPaths {
                ceo_inbox: paths.ceo_inbox.clone(),
                telegram_outbox: paths.telegram_outbox.clone(),
                telegram_outbox_seq: paths.telegram_outbox_seq.clone(),
                telegram_dead_letter: paths.telegram_dead_letter.clone(),
            };

            let result = catch_unwind(AssertUnwindSafe(move || {
                run_bot(token, chat_id, cancel, paths, whisper_model, heartbeat, panic_log);
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
                    if attempt < 3 {
                        std::thread::sleep(Duration::from_secs(10));
                    }
                }
            }
        }
        tracing::error!("telegram bot: abandoning after 3 setup panics");
    });

    Some(TelegramHandle { cancel, heartbeat, join })
}

// ---------------------------------------------------------------------------
// Bot loop
// ---------------------------------------------------------------------------

fn run_bot(
    token: String,
    chat_id: String,
    cancel: Arc<AtomicBool>,
    paths: BotPaths,
    whisper_model: String,
    heartbeat: Arc<AtomicU64>,
    panic_log_path: PathBuf,
) {
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
        ceo_inbox: paths.ceo_inbox,
        telegram_outbox: paths.telegram_outbox,
        telegram_outbox_seq: paths.telegram_outbox_seq,
        telegram_dead_letter: paths.telegram_dead_letter,
    };

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
// Inbound: Telegram -> CEO inbox
// ---------------------------------------------------------------------------

fn poll_updates(ctx: &BotCtx, offset: &mut i64) {
    let body = serde_json::json!({
        "offset": *offset,
        "timeout": 2,
        "allowed_updates": ["message"],
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
                handle_voice(ctx, file_id);
            }
            continue;
        }

        // Handle text messages.
        let Some(text) = msg["text"].as_str() else {
            continue;
        };

        tracing::info!(text_len = text.len(), "telegram: received message");
        if let Err(e) = inbox::append_message(&ctx.ceo_inbox, "telegram", text) {
            tracing::warn!(error = %e, "telegram: failed to write to CEO inbox");
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound: CEO outbox -> Telegram
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
        let result = tg_send(ctx, &msg.message);
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
                if let Err(e) = append_dead_letter(&ctx.telegram_dead_letter, &msg, "permanent send failure") {
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
            tracing::warn!(method = method, status = code, body = body, "telegram: API error");
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

/// Process an incoming voice message: download, transcribe locally, forward to CEO.
fn handle_voice(ctx: &BotCtx, file_id: &str) {
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
            if let Err(e) = inbox::append_message(&ctx.ceo_inbox, "telegram", &text) {
                tracing::warn!(error = %e, "telegram: failed to write transcription to CEO inbox");
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
    tracing::debug!(bytes = audio_data.len(), "telegram: ffmpeg converting OGG to WAV");
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
            "-m", model_path,
            "-nt",   // no timestamps
            "-np",   // no extra prints
            "-l", "auto", // auto-detect language
        ])
        .arg(&wav_tmp)
        .output();

    let _ = std::fs::remove_file(&wav_tmp);

    match result {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            tracing::debug!(text = %text, "telegram: whisper-cli output");
            if text.is_empty() { None } else { Some(text) }
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
