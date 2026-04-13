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

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::inbox;

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
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Spawn the Telegram bot in a background thread (fire-and-forget).
///
/// The thread shuts down when `cancel` is set to `true`.
pub fn start(config: &Config, token: String, chat_id: String, cancel: Arc<AtomicBool>) {
    let ceo_inbox = config.ceo_inbox();
    let telegram_outbox = config.telegram_outbox();
    let telegram_outbox_seq = config.telegram_outbox_seq();
    let whisper_model = std::env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| config.whisper_model_path().to_string_lossy().into_owned());

    std::thread::spawn(move || {
        run_bot(
            token,
            chat_id,
            cancel,
            ceo_inbox,
            telegram_outbox,
            telegram_outbox_seq,
            whisper_model,
        );
    });
}

// ---------------------------------------------------------------------------
// Bot loop
// ---------------------------------------------------------------------------

fn run_bot(
    token: String,
    chat_id: String,
    cancel: Arc<AtomicBool>,
    ceo_inbox: PathBuf,
    telegram_outbox: PathBuf,
    telegram_outbox_seq: PathBuf,
    whisper_model: String,
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
        ceo_inbox,
        telegram_outbox,
        telegram_outbox_seq,
    };

    let mut offset: i64 = 0;

    while !cancel.load(Ordering::Relaxed) {
        drain_outbox(&ctx);
        poll_updates(&ctx, &mut offset);
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

    let Some(resp) = tg_post(&ctx.agent, &format!("{}/getUpdates", ctx.base), &body) else {
        return;
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
        if tg_send(ctx, &msg.message) {
            if let Err(e) = inbox::mark_delivered(&ctx.telegram_outbox_seq, msg.seq) {
                tracing::warn!(error = %e, seq = msg.seq, "telegram: failed to mark delivered");
            }
        } else {
            // Stop on first failure, retry next cycle.
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Telegram API helpers
// ---------------------------------------------------------------------------

fn tg_post(agent: &ureq::Agent, url: &str, body: &Value) -> Option<Value> {
    let method = url.rsplit('/').next().unwrap_or("?");
    tracing::debug!(method = method, "telegram: API request");

    match agent.post(url).send_json(body) {
        Ok(resp) => match resp.into_json::<Value>() {
            Ok(val) => Some(val),
            Err(e) => {
                tracing::warn!(method = method, error = %e, "telegram: response parse error");
                None
            }
        },
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            tracing::warn!(method = method, status = code, body = body, "telegram: API error");
            None
        }
        Err(e) => {
            tracing::warn!(method = method, error = %e, "telegram: transport error");
            None
        }
    }
}

/// Send a plain-text message. Returns `true` on success.
fn tg_send(ctx: &BotCtx, text: &str) -> bool {
    let body = serde_json::json!({
        "chat_id": ctx.chat_id,
        "text": text,
    });
    let result = tg_post(&ctx.agent, &format!("{}/sendMessage", ctx.base), &body);
    if result.is_some() {
        tracing::info!(text_len = text.len(), "telegram: sent message");
    }
    result.is_some()
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
        tg_send(
            ctx,
            &format!("⚠️ Voice transcription requires `{binary}` but it's not installed. Please install it and try again."),
        );
        return;
    }

    let Some(audio_data) = download_voice(&ctx.agent, &ctx.base, &ctx.file_base, file_id) else {
        tg_send(ctx, "Failed to download voice message.");
        return;
    };

    if !ensure_whisper_model(ctx) {
        return;
    }

    match transcribe_audio(&ctx.whisper_model, &audio_data) {
        Some(text) if !text.is_empty() => {
            tracing::info!(text_len = text.len(), "telegram: transcribed voice message");
            tg_send(ctx, &format!("🎤 {text}"));
            if let Err(e) = inbox::append_message(&ctx.ceo_inbox, "telegram", &text) {
                tracing::warn!(error = %e, "telegram: failed to write transcription to CEO inbox");
            }
        }
        _ => {
            tg_send(ctx, "Failed to transcribe voice message.");
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
    let resp = tg_post(agent, &format!("{base}/getFile"), &body)?;
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
    tg_send(ctx, "Downloading whisper model (first time only)...");

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
                    tg_send(ctx, "Failed to save whisper model.");
                    return false;
                }
            };
            if let Err(e) = std::io::copy(&mut resp.into_reader(), &mut file) {
                tracing::error!(error = %e, "telegram: failed to write model file");
                let _ = std::fs::remove_file(&ctx.whisper_model);
                tg_send(ctx, "Failed to download whisper model.");
                return false;
            }
            tracing::info!("telegram: whisper model downloaded successfully");
            true
        }
        Err(e) => {
            tracing::error!(error = %e, "telegram: model download error");
            tg_send(ctx, "Failed to download whisper model.");
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
