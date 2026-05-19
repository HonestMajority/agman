//! Idempotent legacy migrations.
//!
//! Runs at the top of `Config::ensure_dirs()` so every launch picks up any
//! leftover state. All steps are best-effort: warnings are logged but the
//! migration never aborts startup unless both legacy and new state collide.
//!
//! Steps:
//! 1. Rename `~/.agman/ceo/` → `~/.agman/chief-of-staff/` (bail if both exist).
//! 2. Move old researcher records into `~/.agman/agents/`, rewrite their
//!    `meta.json` files to the kind-discriminated shape, then purge any old
//!    global agent dirs (`ceo--*` or `chief-of-staff--*`).
//! 3. Rewrite `~/.agman/telegram/current-agent` if it points to `"ceo"` or
//!    a global agent.
//! 4. Kill the legacy `agman-ceo` tmux session if it is still running.
//!
//! Inbox JSONL files are explicitly NOT rewritten — they are append-only logs.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::config::Config;

/// Run all idempotent legacy migrations.
pub fn run(config: &Config) -> Result<()> {
    migrate_ceo_dir(config)?;
    migrate_legacy_agents_dir(config)?;
    crate::use_cases::purge_chief_of_staff_agents(config);
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

/// Step 2a: move old researcher directories to `~/.agman/agents/` and rewrite
/// their records to the current kind-discriminated shape.
fn migrate_legacy_agents_dir(config: &Config) -> Result<()> {
    let researchers = config.base_dir.join("researchers");
    let new = config.agents_dir();

    if !researchers.exists() {
        rewrite_legacy_agent_meta_files(&new);
        return Ok(());
    }
    if new.exists() {
        bail!(
            "migration: both legacy {} and new {} exist — refusing to merge automatically",
            researchers.display(),
            new.display()
        );
    }

    std::fs::rename(&researchers, &new).with_context(|| {
        format!(
            "failed to rename {} to {}",
            researchers.display(),
            new.display()
        )
    })?;
    tracing::info!(
        from = %researchers.display(),
        to = %new.display(),
        "migration: renamed legacy agent dir to agents"
    );

    rewrite_legacy_agent_meta_files(&new);
    Ok(())
}

fn rewrite_legacy_agent_meta_files(new: &Path) {
    // Rewrite each meta.json into the new shape. Best-effort; per-entry
    // failures are logged but do not abort startup.
    let entries = match std::fs::read_dir(new) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, dir = %new.display(), "migration: failed to read agents dir");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Err(e) = rewrite_legacy_agent_meta(&path) {
            tracing::warn!(
                error = %e,
                dir = %path.display(),
                "migration: failed to rewrite agent meta.json"
            );
        }
    }
}

/// Rewrite a legacy `ResearcherMeta`-shaped `meta.json` to the new
/// `AgentMeta` shape with `kind: Researcher`. Idempotent — files already
/// in the new shape are left alone.
fn rewrite_legacy_agent_meta(dir: &Path) -> Result<()> {
    let meta_path = dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;

    // Already in the new shape — nothing to do.
    if value.get("kind").is_some() {
        return Ok(());
    }

    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return Ok(()),
    };

    let repo = obj.remove("repo").unwrap_or(serde_json::Value::Null);
    let branch = obj.remove("branch").unwrap_or(serde_json::Value::Null);
    let task_id = obj.remove("task_id").unwrap_or(serde_json::Value::Null);

    obj.insert(
        "kind".to_string(),
        serde_json::json!({
            "type": "researcher",
            "repo": repo,
            "branch": branch,
            "task_id": task_id,
        }),
    );

    let new_contents =
        serde_json::to_string_pretty(&value).context("failed to serialize updated agent meta")?;
    std::fs::write(&meta_path, new_contents)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;
    tracing::info!(path = %meta_path.display(), "migration: rewrote agent meta.json to new kind shape");
    Ok(())
}

/// Step 3: rewrite `~/.agman/telegram/current-agent` if it points at the
/// legacy CEO id or a global agent.
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

    let new_value = if value == "ceo" || is_global_agent_ref(value) {
        Some("chief-of-staff".to_string())
    } else {
        None
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

fn is_global_agent_ref(value: &str) -> bool {
    ["researcher:", "operator:", "reviewer:", "tester:"]
        .iter()
        .filter_map(|prefix| value.strip_prefix(prefix))
        .any(|rest| rest.starts_with("ceo--") || rest.starts_with("chief-of-staff--"))
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
