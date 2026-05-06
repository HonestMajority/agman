use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

fn default_now() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AssistantStatus {
    Running,
    Archived,
}

/// One worktree entry tracked by a Reviewer assistant.
///
/// `agman_created` records whether agman set up the worktree (and the local
/// branch) itself — drives archive cleanup. Worktrees that already existed
/// when the reviewer was created are left intact on archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerWorktree {
    pub repo: String,
    pub branch: String,
    pub path: PathBuf,
    pub agman_created: bool,
}

/// Discriminator for the two assistant kinds. Each kind carries its own
/// kind-specific metadata; everything else (harness stamping, inbox, tmux,
/// telegram switcher, send-message routing, poll-target enumeration) is
/// kind-agnostic and lives on the surrounding [`AssistantMeta`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AssistantKind {
    Researcher {
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        task_id: Option<String>,
    },
    Reviewer {
        #[serde(default)]
        worktrees: Vec<ReviewerWorktree>,
    },
}

/// Metadata stored in `~/.agman/assistants/<project>--<name>/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMeta {
    pub name: String,
    pub project: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_now")]
    pub updated_at: DateTime<Utc>,
    pub status: AssistantStatus,
    pub kind: AssistantKind,
}

/// A loaded assistant with its directory path.
#[derive(Debug, Clone)]
pub struct Assistant {
    pub meta: AssistantMeta,
    pub dir: PathBuf,
}

impl Assistant {
    /// Create a new assistant on disk.
    ///
    /// Callers in `use_cases` decide the kind. Researcher kind preserves the
    /// pre-existing behavior (optional repo/branch/task_id); Reviewer kind
    /// records the resolved worktree list (see `create_assistant` for the
    /// worktree resolution rules).
    pub fn create(
        config: &Config,
        project: &str,
        name: &str,
        description: &str,
        kind: AssistantKind,
    ) -> Result<Self> {
        validate_assistant_name(name)?;

        let dir = config.assistant_dir(project, name);
        if dir.exists() {
            bail!("assistant '{name}' already exists in project '{project}'");
        }

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create assistant dir {}", dir.display()))?;

        let meta = AssistantMeta {
            name: name.to_string(),
            project: project.to_string(),
            description: description.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            status: AssistantStatus::Running,
            kind,
        };

        let mut assistant = Self { meta, dir };
        assistant.save_meta()?;
        Ok(assistant)
    }

    /// Load an assistant from its directory.
    pub fn load(dir: PathBuf) -> Result<Self> {
        let meta_path = dir.join("meta.json");
        let contents = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("failed to read {}", meta_path.display()))?;
        let meta: AssistantMeta = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", meta_path.display()))?;
        Ok(Self { meta, dir })
    }

    /// List all assistants across all projects.
    pub fn list_all(config: &Config) -> Result<Vec<Self>> {
        let assistants_dir = config.assistants_dir();
        if !assistants_dir.exists() {
            return Ok(Vec::new());
        }

        let mut assistants = Vec::new();
        for entry in std::fs::read_dir(&assistants_dir)
            .with_context(|| format!("failed to read {}", assistants_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("meta.json").exists() {
                match Self::load(path) {
                    Ok(assistant) => assistants.push(assistant),
                    Err(e) => {
                        tracing::warn!(error = %e, "skipping invalid assistant directory");
                    }
                }
            }
        }
        assistants.sort_by(|a, b| {
            let status_ord = |s: &AssistantStatus| match s {
                AssistantStatus::Running => 0,
                AssistantStatus::Archived => 1,
            };
            status_ord(&a.meta.status)
                .cmp(&status_ord(&b.meta.status))
                .then_with(|| b.meta.updated_at.cmp(&a.meta.updated_at))
        });
        Ok(assistants)
    }

    /// List assistants for a specific project.
    pub fn list_for_project(config: &Config, project: &str) -> Result<Vec<Self>> {
        let all = Self::list_all(config)?;
        Ok(all
            .into_iter()
            .filter(|a| a.meta.project == project)
            .collect())
    }

    /// True if this assistant is a Researcher.
    pub fn is_researcher(&self) -> bool {
        matches!(self.meta.kind, AssistantKind::Researcher { .. })
    }

    /// True if this assistant is a Reviewer.
    pub fn is_reviewer(&self) -> bool {
        matches!(self.meta.kind, AssistantKind::Reviewer { .. })
    }

    /// Write meta.json to disk. Stamps `updated_at` before writing.
    pub fn save_meta(&mut self) -> Result<()> {
        self.meta.updated_at = Utc::now();
        let meta_path = self.dir.join("meta.json");
        let contents =
            serde_json::to_string_pretty(&self.meta).context("failed to serialize assistant meta")?;
        std::fs::write(&meta_path, contents)
            .with_context(|| format!("failed to write {}", meta_path.display()))?;
        Ok(())
    }
}

/// Validate assistant name: alphanumeric, hyphens, and underscores only.
fn validate_assistant_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("assistant name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "assistant name '{}' is invalid: only alphanumeric characters, hyphens, and underscores are allowed",
            name
        );
    }
    Ok(())
}
