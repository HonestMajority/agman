use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ResearcherStatus {
    Running,
    Archived,
}

/// Metadata stored in `~/.agman/researchers/<project>--<name>/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearcherMeta {
    pub name: String,
    pub project: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub status: ResearcherStatus,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub task_id: Option<String>,
}

/// A loaded researcher with its directory path.
#[derive(Debug, Clone)]
pub struct Researcher {
    pub meta: ResearcherMeta,
    pub dir: PathBuf,
}

impl Researcher {
    /// Create a new researcher on disk.
    pub fn create(
        config: &Config,
        project: &str,
        name: &str,
        description: &str,
        repo: Option<String>,
        branch: Option<String>,
        task_id: Option<String>,
    ) -> Result<Self> {
        validate_researcher_name(name)?;

        let dir = config.researcher_dir(project, name);
        if dir.exists() {
            bail!("researcher '{name}' already exists in project '{project}'");
        }

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create researcher dir {}", dir.display()))?;

        let meta = ResearcherMeta {
            name: name.to_string(),
            project: project.to_string(),
            description: description.to_string(),
            created_at: Utc::now(),
            status: ResearcherStatus::Running,
            repo,
            branch,
            task_id,
        };

        let researcher = Self { meta, dir };
        researcher.save_meta()?;
        Ok(researcher)
    }

    /// Load a researcher from its directory.
    pub fn load(dir: PathBuf) -> Result<Self> {
        let meta_path = dir.join("meta.json");
        let contents = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("failed to read {}", meta_path.display()))?;
        let meta: ResearcherMeta = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", meta_path.display()))?;
        Ok(Self { meta, dir })
    }

    /// List all researchers across all projects.
    pub fn list_all(config: &Config) -> Result<Vec<Self>> {
        let researchers_dir = config.researchers_dir();
        if !researchers_dir.exists() {
            return Ok(Vec::new());
        }

        let mut researchers = Vec::new();
        for entry in std::fs::read_dir(&researchers_dir)
            .with_context(|| format!("failed to read {}", researchers_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("meta.json").exists() {
                match Self::load(path) {
                    Ok(researcher) => researchers.push(researcher),
                    Err(e) => {
                        tracing::warn!(error = %e, "skipping invalid researcher directory");
                    }
                }
            }
        }
        researchers.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        Ok(researchers)
    }

    /// List researchers for a specific project.
    pub fn list_for_project(config: &Config, project: &str) -> Result<Vec<Self>> {
        let all = Self::list_all(config)?;
        Ok(all
            .into_iter()
            .filter(|r| r.meta.project == project)
            .collect())
    }

    /// Write meta.json to disk.
    pub fn save_meta(&self) -> Result<()> {
        let meta_path = self.dir.join("meta.json");
        let contents = serde_json::to_string_pretty(&self.meta)
            .context("failed to serialize researcher meta")?;
        std::fs::write(&meta_path, contents)
            .with_context(|| format!("failed to write {}", meta_path.display()))?;
        Ok(())
    }
}

/// Validate researcher name: alphanumeric, hyphens, and underscores only.
fn validate_researcher_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("researcher name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "researcher name '{}' is invalid: only alphanumeric characters, hyphens, and underscores are allowed",
            name
        );
    }
    Ok(())
}
