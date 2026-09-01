use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

/// A single repo entry within a task. For single-repo tasks there is exactly one;
/// for multi-repo tasks there is one per repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub repo_name: String,
    pub worktree_path: PathBuf,
    pub tmux_session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    /// For single-repo tasks this equals the repo name; for multi-repo tasks
    /// it is the parent directory name (e.g. "repos").
    pub name: String,
    pub branch_name: String,
    /// All repos involved in this task. Single-repo tasks have exactly one entry.
    pub repos: Vec<RepoEntry>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The parent directory containing the repo(s), when it differs from `config.repos_dir`.
    /// Set for multi-repo tasks (always) and single-repo tasks whose repo lives outside `repos_dir`.
    /// `None` when the repo is under `config.repos_dir`.
    #[serde(default)]
    pub parent_dir: Option<PathBuf>,
    /// Whether this is a multi-repo task (multiple repos sharing one branch).
    /// Distinct from `parent_dir` which can also be set for single-repo tasks
    /// whose repo lives outside `repos_dir`.
    /// `None` for tasks created before this field existed — falls back to
    /// `parent_dir.is_some()` for backward compatibility.
    #[serde(default)]
    pub multi_repo: Option<bool>,
    /// Linked GitHub PR (number + URL), populated when a PR is created
    #[serde(default)]
    pub linked_pr: Option<LinkedPr>,
    /// When set, this task is archived (removed from active list, but directory kept).
    /// The timestamp records when the task was archived.
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
    /// When true, this archived task is exempt from auto-purge.
    #[serde(default)]
    pub saved: bool,
    /// The project this task belongs to (None for unassigned/legacy tasks).
    #[serde(default)]
    pub project: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPr {
    pub number: u64,
    pub url: String,
    #[serde(default = "default_true")]
    pub owned: bool,
    #[serde(default)]
    pub author: Option<String>,
}

impl TaskMeta {
    pub fn new(
        name: String,
        branch_name: String,
        worktree_path: PathBuf,
        _launch_mode: String,
    ) -> Self {
        let now = Utc::now();
        let tmux_session = Config::tmux_session_name(&name, &branch_name);
        let repo_entry = RepoEntry {
            repo_name: name.clone(),
            worktree_path,
            tmux_session,
        };
        Self {
            name,
            branch_name,
            repos: vec![repo_entry],
            created_at: now,
            updated_at: now,
            parent_dir: None,
            multi_repo: Some(false),
            linked_pr: None,
            archived_at: None,
            saved: false,
            project: None,
        }
    }

    /// Create a TaskMeta for a multi-repo task (starts with empty repos).
    pub fn new_multi(
        name: String,
        branch_name: String,
        parent_dir: PathBuf,
        _launch_mode: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            name,
            branch_name,
            repos: vec![],
            created_at: now,
            updated_at: now,
            parent_dir: Some(parent_dir),
            multi_repo: Some(true),
            linked_pr: None,
            archived_at: None,
            saved: false,
            project: None,
        }
    }

    /// Get the task ID (name--branch format)
    pub fn task_id(&self) -> String {
        Config::task_id(&self.name, &self.branch_name)
    }

    /// Get the primary (first) repo entry. Panics if repos is empty.
    pub fn primary_repo(&self) -> &RepoEntry {
        &self.repos[0]
    }

    /// Whether this is a multi-repo task.
    /// Uses the explicit `multi_repo` field when present. Falls back to
    /// `parent_dir.is_some()` for tasks created before the field existed.
    pub fn is_multi_repo(&self) -> bool {
        match self.multi_repo {
            Some(v) => v,
            // Backward compat: tasks created before `multi_repo` was added
            // used `parent_dir` exclusively for multi-repo tasks.
            None => self.parent_dir.is_some(),
        }
    }

    /// Returns true if repos have been populated (safe to call `primary_repo()`).
    pub fn has_repos(&self) -> bool {
        !self.repos.is_empty()
    }
}

#[derive(Debug)]
pub struct Task {
    pub meta: TaskMeta,
    pub dir: PathBuf,
}

impl Task {
    pub fn create(
        config: &Config,
        name: &str,
        branch_name: &str,
        _first_prompt: &str,
        launch_mode: &str,
        worktree_path: PathBuf,
    ) -> Result<Self> {
        tracing::info!(
            repo = name,
            branch = branch_name,
            launch_mode,
            "creating task"
        );
        let dir = config.task_dir(name, branch_name);
        std::fs::create_dir_all(&dir).context("Failed to create task directory")?;

        let meta = TaskMeta::new(
            name.to_string(),
            branch_name.to_string(),
            worktree_path.clone(),
            launch_mode.to_string(),
        );

        let task = Self { meta, dir };
        task.save_meta()?;

        Ok(task)
    }

    /// Create a multi-repo task: task dir only; repos are added separately.
    pub fn create_multi(
        config: &Config,
        name: &str,
        branch_name: &str,
        _first_prompt: &str,
        launch_mode: &str,
        parent_dir: PathBuf,
    ) -> Result<Self> {
        tracing::info!(
            name = name,
            branch = branch_name,
            launch_mode,
            "creating multi-repo task"
        );
        let dir = config.task_dir(name, branch_name);
        std::fs::create_dir_all(&dir).context("Failed to create task directory")?;

        let meta = TaskMeta::new_multi(
            name.to_string(),
            branch_name.to_string(),
            parent_dir,
            launch_mode.to_string(),
        );

        let task = Self { meta, dir };
        task.save_meta()?;

        Ok(task)
    }

    pub fn load(config: &Config, repo_name: &str, branch_name: &str) -> Result<Self> {
        let dir = config.task_dir(repo_name, branch_name);
        if !dir.exists() {
            anyhow::bail!("Task '{}--{}' does not exist", repo_name, branch_name);
        }

        let meta_path = dir.join("meta.json");
        let meta_content =
            std::fs::read_to_string(&meta_path).context("Failed to read task meta.json")?;
        let meta: TaskMeta =
            serde_json::from_str(&meta_content).context("Failed to parse task meta.json")?;

        let task = Self { meta, dir };

        Ok(task)
    }

    /// Load a task by task_id (repo--branch format)
    pub fn load_by_id(config: &Config, task_id: &str) -> Result<Self> {
        if let Some((repo_name, branch_name)) = Config::parse_task_id(task_id) {
            Self::load(config, &repo_name, &branch_name)
        } else {
            // Try to find a task that matches just the branch name
            let tasks = Self::list_all(config);
            let matching: Vec<_> = tasks
                .into_iter()
                .filter(|t| t.meta.branch_name == task_id)
                .collect();

            match matching.len() {
                0 => anyhow::bail!("Task '{}' not found", task_id),
                1 => Ok(matching.into_iter().next().unwrap()),
                _ => anyhow::bail!(
                    "Ambiguous task '{}' - found in multiple repos. Use repo--branch format.",
                    task_id
                ),
            }
        }
    }

    pub fn list_all(config: &Config) -> Vec<Task> {
        let mut tasks = Vec::new();

        if !config.tasks_dir.exists() {
            return tasks;
        }

        let read_dir = match std::fs::read_dir(&config.tasks_dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(path = %config.tasks_dir.display(), error = %e, "failed to read tasks directory");
                return tasks;
            }
        };

        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read task directory entry");
                    continue;
                }
            };
            let is_dir = match entry.file_type() {
                Ok(ft) => ft.is_dir(),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to get file type for task entry");
                    continue;
                }
            };
            if is_dir {
                let task_id = entry.file_name().to_string_lossy().to_string();
                if let Some((repo_name, branch_name)) = Config::parse_task_id(&task_id) {
                    match Task::load(config, &repo_name, &branch_name) {
                        Ok(task) => {
                            // Filter out archived tasks
                            if task.meta.archived_at.is_none() {
                                tasks.push(task);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(task_id = %task_id, error = %e, "failed to load task");
                        }
                    }
                }
            }
        }

        tasks.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));

        tasks
    }

    /// List all archived tasks, sorted by `archived_at` descending (most recently archived first).
    pub fn list_archived(config: &Config) -> Vec<Task> {
        let mut tasks = Vec::new();

        if !config.tasks_dir.exists() {
            return tasks;
        }

        let read_dir = match std::fs::read_dir(&config.tasks_dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(path = %config.tasks_dir.display(), error = %e, "failed to read tasks directory");
                return tasks;
            }
        };

        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                let task_id = entry.file_name().to_string_lossy().to_string();
                if let Some((repo_name, branch_name)) = Config::parse_task_id(&task_id) {
                    match Task::load(config, &repo_name, &branch_name) {
                        Ok(task) => {
                            if task.meta.archived_at.is_some() {
                                tasks.push(task);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(task_id = %task_id, error = %e, "failed to load task");
                        }
                    }
                }
            }
        }

        // Sort by archived_at descending (most recently archived first)
        tasks.sort_by(|a, b| b.meta.archived_at.cmp(&a.meta.archived_at));

        tasks
    }

    pub fn save_meta(&self) -> Result<()> {
        let meta_path = self.dir.join("meta.json");
        let content = serde_json::to_string_pretty(&self.meta)?;
        std::fs::write(&meta_path, content)?;
        Ok(())
    }

    pub fn delete(self, config: &Config) -> Result<()> {
        tracing::info!(task_id = %self.meta.task_id(), "deleting task");
        let dir = config.task_dir(&self.meta.name, &self.meta.branch_name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    pub fn time_since_update(&self) -> String {
        let duration = Utc::now().signed_duration_since(self.meta.updated_at);

        if duration.num_days() > 0 {
            format!("{}d ago", duration.num_days())
        } else if duration.num_hours() > 0 {
            format!("{}h ago", duration.num_hours())
        } else if duration.num_minutes() > 0 {
            format!("{}m ago", duration.num_minutes())
        } else {
            "just now".to_string()
        }
    }

    /// Check if this archived task has expired based on the retention period.
    pub fn is_archive_expired(&self, retention_days: u64) -> bool {
        match self.meta.archived_at {
            Some(archived_at) if !self.meta.saved => {
                let retention = Duration::days(retention_days as i64);
                Utc::now().signed_duration_since(archived_at) > retention
            }
            _ => false,
        }
    }

    pub fn set_linked_pr(
        &mut self,
        number: u64,
        url: String,
        owned: bool,
        author: Option<String>,
    ) -> Result<()> {
        self.meta.linked_pr = Some(LinkedPr {
            number,
            url,
            owned,
            author,
        });
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }
}
