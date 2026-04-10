//! Minimal Telegram bot integration for the CEO agent.
//!
//! When `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` env vars are set, a
//! background thread bridges plain-text messages between the user's Telegram
//! chat and the CEO tmux session via the existing inbox/outbox JSONL files.
//!
//! If the env vars are absent the feature is completely dormant.

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
    chat_id: String,
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

    std::thread::spawn(move || {
        run_bot(token, chat_id, cancel, ceo_inbox, telegram_outbox, telegram_outbox_seq);
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
) {
    tracing::info!(chat_id = %chat_id, token_len = token.len(), "telegram bot starting");

    let ctx = BotCtx {
        agent: ureq::AgentBuilder::new()
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .build(),
        base: format!("https://api.telegram.org/bot{token}"),
        chat_id,
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

        // Only handle text messages from the configured chat.
        let Some(msg) = update.get("message") else {
            continue;
        };
        let Some(msg_chat_id) = msg["chat"]["id"].as_i64() else {
            continue;
        };
        if msg_chat_id.to_string() != ctx.chat_id {
            continue;
        }
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
