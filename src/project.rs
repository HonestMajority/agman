use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

/// Metadata stored in `~/.agman/projects/<name>/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub held: bool,
}

/// A loaded project with its directory path.
#[derive(Debug, Clone)]
pub struct Project {
    pub meta: ProjectMeta,
    pub dir: PathBuf,
}

impl Project {
    /// Create a new project on disk.
    pub fn create(config: &Config, name: &str, description: &str) -> Result<Self> {
        validate_project_name(name)?;

        let dir = config.project_dir(name);
        if dir.exists() {
            bail!("project '{}' already exists", name);
        }

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create project dir {}", dir.display()))?;

        let meta = ProjectMeta {
            name: name.to_string(),
            description: description.to_string(),
            created_at: Utc::now(),
            held: false,
        };

        let project = Self { meta, dir };
        project.save_meta()?;
        Ok(project)
    }

    /// Load a project from its directory.
    pub fn load(dir: PathBuf) -> Result<Self> {
        let meta_path = dir.join("meta.json");
        let contents = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("failed to read {}", meta_path.display()))?;
        let meta: ProjectMeta = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", meta_path.display()))?;
        Ok(Self { meta, dir })
    }

    /// Load a project by name.
    pub fn load_by_name(config: &Config, name: &str) -> Result<Self> {
        let dir = config.project_dir(name);
        Self::load(dir)
    }

    /// List all projects.
    pub fn list_all(config: &Config) -> Result<Vec<Self>> {
        let projects_dir = config.projects_dir();
        if !projects_dir.exists() {
            return Ok(Vec::new());
        }

        let mut projects = Vec::new();
        for entry in std::fs::read_dir(&projects_dir)
            .with_context(|| format!("failed to read {}", projects_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("meta.json").exists() {
                match Self::load(path) {
                    Ok(project) => projects.push(project),
                    Err(e) => {
                        tracing::warn!(error = %e, "skipping invalid project directory");
                    }
                }
            }
        }
        projects.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        Ok(projects)
    }

    /// Write meta.json to disk.
    pub fn save_meta(&self) -> Result<()> {
        let meta_path = self.dir.join("meta.json");
        let contents = serde_json::to_string_pretty(&self.meta)
            .context("failed to serialize project meta")?;
        std::fs::write(&meta_path, contents)
            .with_context(|| format!("failed to write {}", meta_path.display()))?;
        Ok(())
    }
}

/// Validate project name: alphanumeric, hyphens, and underscores only.
fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("project name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "project name '{}' is invalid: only alphanumeric characters, hyphens, and underscores are allowed",
            name
        );
    }
    Ok(())
}
