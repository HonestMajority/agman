use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single message in an agent's inbox (one JSONL line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub seq: u64,
    pub from: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

/// Append a message to an inbox JSONL file. Assigns the next sequence number
/// automatically by reading the current max seq from the file.
pub fn append_message(inbox_path: &Path, from: &str, message: &str) -> Result<InboxMessage> {
    // Read existing messages to determine next seq
    let next_seq = match read_messages(inbox_path) {
        Ok(messages) => messages.last().map_or(1, |m| m.seq + 1),
        Err(_) => 1,
    };

    let msg = InboxMessage {
        seq: next_seq,
        from: from.to_string(),
        message: message.to_string(),
        timestamp: Utc::now(),
    };

    let line = serde_json::to_string(&msg).context("failed to serialize inbox message")?;

    // Ensure parent directory exists
    if let Some(parent) = inbox_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create inbox dir {}", parent.display()))?;
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_path)
        .with_context(|| format!("failed to open inbox {}", inbox_path.display()))?;
    writeln!(file, "{}", line)
        .with_context(|| format!("failed to write to inbox {}", inbox_path.display()))?;

    Ok(msg)
}

/// Read all messages from an inbox JSONL file.
pub fn read_messages(inbox_path: &Path) -> Result<Vec<InboxMessage>> {
    if !inbox_path.exists() {
        return Ok(Vec::new());
    }

    let contents = std::fs::read_to_string(inbox_path)
        .with_context(|| format!("failed to read inbox {}", inbox_path.display()))?;

    let mut messages = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: InboxMessage = serde_json::from_str(line)
            .with_context(|| format!("failed to parse inbox line: {}", line))?;
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
