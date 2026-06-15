use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

/// A single message in an agent's inbox (one JSONL line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub seq: u64,
    pub from: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

/// Serializes concurrent `append_message` calls inside one process. The
/// per-inbox lock file in `append_message` is the cross-process guard.
static APPEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Append a message to an inbox JSONL file. Assigns the next sequence number
/// automatically by reading the current max seq from the file.
///
/// Safe across agman processes that use this function. Parse/read errors are
/// returned to the caller and never treated as an empty inbox.
pub fn append_message(inbox_path: &Path, from: &str, message: &str) -> Result<InboxMessage> {
    // Ensure parent directory exists before taking the lock file.
    if let Some(parent) = inbox_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create inbox dir {}", parent.display()))?;
    }

    let _guard = APPEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let lock_path = inbox_lock_path(inbox_path);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open inbox lock {}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("failed to lock inbox {}", inbox_path.display()))?;

    // Hold the inter-process lock across read-max-seq and append.
    let messages = read_messages(inbox_path)?;
    let next_seq = messages
        .iter()
        .map(|m| m.seq)
        .max()
        .map_or(1, |seq| seq + 1);

    let msg = InboxMessage {
        seq: next_seq,
        from: from.to_string(),
        message: message.to_string(),
        timestamp: Utc::now(),
    };

    let mut line = serde_json::to_vec(&msg).context("failed to serialize inbox message")?;
    line.push(b'\n');

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_path)
        .with_context(|| format!("failed to open inbox {}", inbox_path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("failed to write to inbox {}", inbox_path.display()))?;

    Ok(msg)
}

fn inbox_lock_path(inbox_path: &Path) -> std::path::PathBuf {
    let mut lock_name = inbox_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("inbox.jsonl"));
    lock_name.push(".lock");
    inbox_path.with_file_name(lock_name)
}

/// Read all messages from an inbox JSONL file.
pub fn read_messages(inbox_path: &Path) -> Result<Vec<InboxMessage>> {
    if !inbox_path.exists() {
        return Ok(Vec::new());
    }

    let contents = std::fs::read_to_string(inbox_path)
        .with_context(|| format!("failed to read inbox {}", inbox_path.display()))?;

    let mut messages = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let line_number = index + 1;
        let msg: InboxMessage = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse inbox {} at line {}",
                inbox_path.display(),
                line_number
            )
        })?;
        messages.push(msg);
    }
    Ok(messages)
}

/// Read the last delivered sequence number from the seq tracking file.
pub fn read_last_delivered(seq_path: &Path) -> Result<u64> {
    if !seq_path.exists() {
        return Ok(0);
    }
    let contents = std::fs::read_to_string(seq_path)
        .with_context(|| format!("failed to read seq file {}", seq_path.display()))?;
    let seq: u64 = contents
        .trim()
        .parse()
        .with_context(|| format!("failed to parse seq file: '{}'", contents.trim()))?;
    Ok(seq)
}

/// Read messages that haven't been delivered yet (seq > last delivered).
pub fn read_undelivered(inbox_path: &Path, seq_path: &Path) -> Result<Vec<InboxMessage>> {
    let last_delivered = read_last_delivered(seq_path)?;
    let messages = read_messages(inbox_path)?;
    Ok(messages
        .into_iter()
        .filter(|m| m.seq > last_delivered)
        .collect())
}

/// Update the last delivered sequence number.
pub fn mark_delivered(seq_path: &Path, seq: u64) -> Result<()> {
    if let Some(parent) = seq_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create seq dir {}", parent.display()))?;
    }
    std::fs::write(seq_path, seq.to_string())
        .with_context(|| format!("failed to write seq file {}", seq_path.display()))?;
    Ok(())
}
