use anyhow::{Context, Result};
use std::path::Path;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use agman::config::Config;

/// Rotate the log file if it exceeds 1000 lines.
/// Keeps the most recent 750 lines.
pub fn rotate_log(config: &Config) {
    let log_path = config.base_dir.join("agman.log");
    if !log_path.exists() {
        return;
    }

    let content = match std::fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 1000 {
        return;
    }

    // Keep the most recent 750 lines
    let start = lines.len() - 750;
    let trimmed = lines[start..].join("\n");
    let _ = std::fs::write(&log_path, format!("{}\n", trimmed));
}

/// Set up file-based logging with tracing-subscriber.
///
/// Logs go to `~/.agman/agman.log` with human-readable format.
/// Default level: DEBUG for agman modules, WARN for dependencies.
/// Stderr output is suppressed so the TUI is not affected.
pub fn setup_logging(config: &Config) -> Result<()> {
    // Ensure the base directory exists
    std::fs::create_dir_all(&config.base_dir)
        .context("Failed to create agman base directory for logging")?;

    let log_path = config.base_dir.join("agman.log");
    let log_file = open_log_file(&log_path)?;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("agman=debug,warn")
    });

    let file_layer = fmt::layer()
        .with_writer(log_file)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_thread_ids(false)
        .with_thread_names(false);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .init();

    tracing::debug!("Logging initialized, writing to {}", log_path.display());

    Ok(())
}

fn open_log_file(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open log file: {}", path.display()))
}
