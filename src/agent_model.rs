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
pub enum AgentStatus {
    Running,
    Archived,
}

/// One worktree entry tracked by a worktree-backed agent.
///
/// `agman_created` records whether agman set up the worktree (and the local
/// branch) itself — drives archive cleanup. Worktrees that already existed
/// when the reviewer was created are left intact on archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWorktree {
    pub repo: String,
    pub branch: String,
    pub path: PathBuf,
    pub agman_created: bool,
}

#[derive(Default, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct TesterCapabilities {
    pub browser: bool,
}

/// Attachment state for long-lived agents.
///
/// Engineers are task-owned and must always be attached to exactly one task.
/// Other agent kinds may be unattached project agents or attached to a task
/// with an optional PM-facing role label.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AgentAttachment {
    #[default]
    Unattached,
    Task {
        task_id: String,
        #[serde(default)]
        role_label: Option<String>,
    },
}

/// Discriminator for the agent kinds. Each kind carries its own
/// kind-specific metadata; everything else (harness stamping, inbox, tmux,
/// telegram switcher, send-message routing, poll-target enumeration) is
/// kind-agnostic and lives on the surrounding [`AgentMeta`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AgentKind {
    Engineer,
    Researcher {
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        task_id: Option<String>,
    },
    Operator {
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        task_id: Option<String>,
    },
    Reviewer {
        #[serde(default)]
        worktrees: Vec<AgentWorktree>,
    },
    Tester {
        #[serde(default)]
        worktrees: Vec<AgentWorktree>,
        #[serde(default)]
        capabilities: TesterCapabilities,
    },
}

/// Metadata stored in the agent state directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub name: String,
    pub project: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_now")]
    pub updated_at: DateTime<Utc>,
    pub status: AgentStatus,
    pub kind: AgentKind,
    #[serde(default)]
    pub attachment: AgentAttachment,
}

/// A loaded agent with its directory path.
#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub meta: AgentMeta,
    pub dir: PathBuf,
}

impl AgentRecord {
    /// Create a new agent on disk.
    ///
    /// Callers in `use_cases` decide the kind and attachment.
    pub fn create(
        config: &Config,
        project: &str,
        name: &str,
        description: &str,
        kind: AgentKind,
    ) -> Result<Self> {
        Self::create_with_attachment(
            config,
            project,
            name,
            description,
            kind,
            AgentAttachment::Unattached,
        )
    }

    pub fn create_with_attachment(
        config: &Config,
        project: &str,
        name: &str,
        description: &str,
        kind: AgentKind,
        attachment: AgentAttachment,
    ) -> Result<Self> {
        validate_agent_name(name)?;
        validate_agent_attachment(&kind, &attachment)?;

        let dir = config.agent_dir(project, name);
        if dir.exists() {
            bail!("agent '{name}' already exists in project '{project}'");
        }

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create agent dir {}", dir.display()))?;

        let meta = AgentMeta {
            name: name.to_string(),
            project: project.to_string(),
            description: description.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            status: AgentStatus::Running,
            kind,
            attachment,
        };

        let mut agent = Self { meta, dir };
        agent.save_meta()?;
        Ok(agent)
    }

    /// Load an agent from its directory.
    pub fn load(dir: PathBuf) -> Result<Self> {
        let meta_path = dir.join("meta.json");
        let contents = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("failed to read {}", meta_path.display()))?;
        let meta: AgentMeta = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", meta_path.display()))?;
        Ok(Self { meta, dir })
    }

    /// List all agents across all projects.
    pub fn list_all(config: &Config) -> Result<Vec<Self>> {
        let mut agents = Vec::new();
        let agents_dir = config.agents_dir();
        if agents_dir.exists() {
            for entry in std::fs::read_dir(&agents_dir)
                .with_context(|| format!("failed to read {}", agents_dir.display()))?
            {
                let path = entry?.path();
                if path.is_dir() && path.join("meta.json").exists() {
                    match Self::load(path) {
                        Ok(agent) => agents.push(agent),
                        Err(e) => tracing::warn!(error = %e, "skipping invalid agent directory"),
                    }
                }
            }
        }
        agents.sort_by(|a, b| {
            let status_ord = |s: &AgentStatus| match s {
                AgentStatus::Running => 0,
                AgentStatus::Archived => 1,
            };
            status_ord(&a.meta.status)
                .cmp(&status_ord(&b.meta.status))
                .then_with(|| b.meta.updated_at.cmp(&a.meta.updated_at))
        });
        Ok(agents)
    }

    /// List agents for a specific project.
    pub fn list_for_project(config: &Config, project: &str) -> Result<Vec<Self>> {
        let all = Self::list_all(config)?;
        Ok(all
            .into_iter()
            .filter(|a| a.meta.project == project)
            .collect())
    }

    /// True if this agent is a Researcher.
    pub fn is_researcher(&self) -> bool {
        matches!(self.meta.kind, AgentKind::Researcher { .. })
    }

    /// True if this agent is an Operator.
    pub fn is_operator(&self) -> bool {
        matches!(self.meta.kind, AgentKind::Operator { .. })
    }

    /// True if this agent is a Reviewer.
    pub fn is_reviewer(&self) -> bool {
        matches!(self.meta.kind, AgentKind::Reviewer { .. })
    }

    /// True if this agent is a Tester.
    pub fn is_tester(&self) -> bool {
        matches!(self.meta.kind, AgentKind::Tester { .. })
    }

    /// True if this agent is the task-owned Engineer.
    pub fn is_engineer(&self) -> bool {
        matches!(self.meta.kind, AgentKind::Engineer)
    }

    /// Set this agent's attachment and persist the metadata.
    pub fn set_attachment(&mut self, attachment: AgentAttachment) -> Result<()> {
        validate_agent_attachment(&self.meta.kind, &attachment)?;
        self.meta.attachment = attachment;
        self.save_meta()
    }

    /// Write meta.json to disk. Stamps `updated_at` before writing.
    pub fn save_meta(&mut self) -> Result<()> {
        self.meta.updated_at = Utc::now();
        let meta_path = self.dir.join("meta.json");
        let contents =
            serde_json::to_string_pretty(&self.meta).context("failed to serialize agent meta")?;
        std::fs::write(&meta_path, contents)
            .with_context(|| format!("failed to write {}", meta_path.display()))?;
        Ok(())
    }
}

pub fn validate_agent_attachment(kind: &AgentKind, attachment: &AgentAttachment) -> Result<()> {
    match (kind, attachment) {
        (AgentKind::Engineer, AgentAttachment::Task { task_id, .. }) if !task_id.is_empty() => {
            Ok(())
        }
        (AgentKind::Engineer, _) => {
            bail!("engineer agents must be attached to exactly one task")
        }
        (_, AgentAttachment::Task { task_id, .. }) if task_id.is_empty() => {
            bail!("task attachment requires a non-empty task_id")
        }
        _ => Ok(()),
    }
}

/// Validate agent name: alphanumeric, hyphens, and underscores only.
fn validate_agent_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("agent name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "agent name '{}' is invalid: only alphanumeric characters, hyphens, and underscores are allowed",
            name
        );
    }
    Ok(())
}
