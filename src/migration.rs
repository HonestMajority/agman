//! Idempotent legacy CEO → Chief of Staff migration.
//!
//! Runs at the top of `Config::ensure_dirs()` so every launch picks up any
//! leftover state. All steps are best-effort: warnings are logged but the
//! migration never aborts startup unless both legacy and new state collide.
//!
//! Steps:
//! 1. Rename `~/.agman/ceo/` → `~/.agman/chief-of-staff/` (bail if both exist).
//! 2. Rename researcher dirs `ceo--<name>/` → `chief-of-staff--<name>/` and
//!    rewrite the `project` field inside each `meta.json`.
//! 3. Rewrite `~/.agman/telegram/current-agent` if it points to `"ceo"` or
//!    `"researcher:ceo--*"`.
//! 4. Kill the legacy `agman-ceo` tmux session if it is still running.
//!
//! Inbox JSONL files are explicitly NOT rewritten — they are append-only logs.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::config::Config;

/// Run the CEO → Chief of Staff migration. Idempotent.
pub fn run(config: &Config) -> Result<()> {
    migrate_ceo_dir(config)?;
    migrate_researcher_dirs(config)?;
    migrate_telegram_current_agent(config)?;
    kill_legacy_tmux_session();
    Ok(())
}

/// Step 1: rename `~/.agman/ceo/` → `~/.agman/chief-of-staff/`.
fn migrate_ceo_dir(config: &Config) -> Result<()> {
    let legacy = config.base_dir.join("ceo");
    let new = config.chief_of_staff_dir();

    if !legacy.exists() {
        return Ok(());
    }
    if new.exists() {
        bail!(
            "migration: both legacy {} and new {} exist — refusing to merge automatically",
            legacy.display(),
            new.display()
        );
    }

    std::fs::rename(&legacy, &new)
        .with_context(|| format!("failed to rename {} to {}", legacy.display(), new.display()))?;
    tracing::info!(
        from = %legacy.display(),
        to = %new.display(),
        "migration: renamed ceo dir to chief-of-staff"
    );
    Ok(())
}

/// Step 2: rename researcher dirs `ceo--*` → `chief-of-staff--*` and rewrite
/// the `project` field in each `meta.json`.
fn migrate_researcher_dirs(config: &Config) -> Result<()> {
    let researchers_dir = config.researchers_dir();
    if !researchers_dir.exists() {
        return Ok(());
    }

    let entries = match std::fs::read_dir(&researchers_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, dir = %researchers_dir.display(), "migration: failed to read researchers dir");
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let Some(rest) = name.strip_prefix("ceo--") else {
            continue;
        };

        let new_name = format!("chief-of-staff--{rest}");
        let new_path = researchers_dir.join(&new_name);

        if new_path.exists() {
            tracing::warn!(
                legacy = %path.display(),
                new = %new_path.display(),
                "migration: target researcher dir already exists, skipping"
            );
            continue;
        }

        if let Err(e) = std::fs::rename(&path, &new_path) {
            tracing::warn!(error = %e, from = %path.display(), to = %new_path.display(), "migration: failed to rename researcher dir");
            continue;
        }
        tracing::info!(
            from = %path.display(),
            to = %new_path.display(),
            "migration: renamed researcher dir"
        );

        if let Err(e) = rewrite_researcher_project(&new_path) {
            tracing::warn!(error = %e, dir = %new_path.display(), "migration: failed to rewrite researcher meta.json");
        }
    }
    Ok(())
}

/// Rewrite `meta.json` in a researcher dir so `project == "chief-of-staff"`.
/// Tolerates missing/malformed files — best-effort.
fn rewrite_researcher_project(dir: &Path) -> Result<()> {
    let meta_path = dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;

    let needs_update = value
        .get("project")
        .and_then(|v| v.as_str())
        .map(|s| s == "ceo")
        .unwrap_or(false);

    if !needs_update {
        return Ok(());
    }

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "project".to_string(),
            serde_json::Value::String("chief-of-staff".to_string()),
        );
    }

    let new_contents = serde_json::to_string_pretty(&value)
        .context("failed to serialize updated researcher meta")?;
    std::fs::write(&meta_path, new_contents)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;
    tracing::info!(path = %meta_path.display(), "migration: updated researcher meta.json project field");
    Ok(())
}

/// Step 3: rewrite `~/.agman/telegram/current-agent` if it points at the
/// legacy CEO id (`"ceo"` or `"researcher:ceo--<name>"`).
fn migrate_telegram_current_agent(config: &Config) -> Result<()> {
    let path = config.telegram_current_agent_path();
    if !path.exists() {
        return Ok(());
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "migration: failed to read telegram current-agent");
            return Ok(());
        }
    };
    let value = raw.trim();

    let new_value = if value == "ceo" {
        Some("chief-of-staff".to_string())
    } else {
        value
            .strip_prefix("researcher:ceo--")
            .map(|rest| format!("researcher:chief-of-staff--{rest}"))
    };

    let Some(new_value) = new_value else {
        return Ok(());
    };

    std::fs::write(&path, &new_value)
        .with_context(|| format!("failed to write {}", path.display()))?;
    tracing::info!(
        path = %path.display(),
        from = value,
        to = %new_value,
        "migration: rewrote telegram current-agent"
    );
    Ok(())
}

/// Step 4: kill the legacy `agman-ceo` tmux session if it is still running.
fn kill_legacy_tmux_session() {
    const LEGACY: &str = "agman-ceo";
    if crate::tmux::Tmux::session_exists(LEGACY) {
        match crate::tmux::Tmux::kill_session(LEGACY) {
            Ok(()) => tracing::info!(session = LEGACY, "migration: killed legacy tmux session"),
            Err(e) => {
                tracing::warn!(error = %e, session = LEGACY, "migration: failed to kill legacy tmux session")
            }
        }
    }
}
