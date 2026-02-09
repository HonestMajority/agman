use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Stopped,
    InputNeeded,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Stopped => write!(f, "stopped"),
            TaskStatus::InputNeeded => write!(f, "input needed"),
        }
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    pub repo_name: String,
    pub branch_name: String,
    pub status: TaskStatus,
    pub tmux_session: String,
    pub worktree_path: PathBuf,
    pub flow_name: String,
    pub current_agent: Option<String>,
    pub flow_step: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Queue of feedback items to be processed when the task stops
    #[serde(default)]
    pub feedback_queue: Vec<String>,
    /// When true, run the review-pr command automatically after the flow completes
    #[serde(default)]
    pub review_after: bool,
}

impl TaskMeta {
    pub fn new(
        repo_name: String,
        branch_name: String,
        worktree_path: PathBuf,
        flow_name: String,
    ) -> Self {
        let now = Utc::now();
        let tmux_session = Config::tmux_session_name(&repo_name, &branch_name);
        Self {
            repo_name,
            branch_name,
            status: TaskStatus::Running,
            tmux_session,
            worktree_path,
            flow_name,
            current_agent: None,
            flow_step: 0,
            created_at: now,
            updated_at: now,
            feedback_queue: Vec::new(),
            review_after: false,
        }
    }

    /// Get the task ID (repo--branch format)
    pub fn task_id(&self) -> String {
        Config::task_id(&self.repo_name, &self.branch_name)
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
        repo_name: &str,
        branch_name: &str,
        description: &str,
        flow_name: &str,
        worktree_path: PathBuf,
    ) -> Result<Self> {
        tracing::info!(repo = repo_name, branch = branch_name, flow = flow_name, "creating task");
        let dir = config.task_dir(repo_name, branch_name);
        std::fs::create_dir_all(&dir).context("Failed to create task directory")?;

        let meta = TaskMeta::new(
            repo_name.to_string(),
            branch_name.to_string(),
            worktree_path.clone(),
            flow_name.to_string(),
        );

        let task = Self { meta, dir };
        task.save_meta()?;
        task.init_files()?;

        // Ensure TASK.md is excluded from git before writing it
        task.ensure_git_excludes_task()?;

        // Write TASK.md directly to the worktree
        task.write_task(&format!(
            "# Goal\n{}\n\n# Plan\n(To be created by planner agent)\n",
            description
        ))?;

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
            let tasks = Self::list_all(config)?;
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

    pub fn list_all(config: &Config) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if !config.tasks_dir.exists() {
            return Ok(tasks);
        }

        for entry in std::fs::read_dir(&config.tasks_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let task_id = entry.file_name().to_string_lossy().to_string();
                if let Some((repo_name, branch_name)) = Config::parse_task_id(&task_id) {
                    match Task::load(config, &repo_name, &branch_name) {
                        Ok(task) => tasks.push(task),
                        Err(e) => {
                            tracing::warn!("Failed to load task '{}': {}", task_id, e);
                        }
                    }
                }
            }
        }

        // Sort by: running first, then input_needed, then stopped; within each group by updated_at desc
        tasks.sort_by(|a, b| {
            let order = |s: TaskStatus| match s {
                TaskStatus::Running => 0,
                TaskStatus::InputNeeded => 1,
                TaskStatus::Stopped => 2,
            };
            let ord = order(a.meta.status).cmp(&order(b.meta.status));
            if ord != std::cmp::Ordering::Equal {
                ord
            } else {
                b.meta.updated_at.cmp(&a.meta.updated_at)
            }
        });

        Ok(tasks)
    }

    pub fn save_meta(&self) -> Result<()> {
        let meta_path = self.dir.join("meta.json");
        let content = serde_json::to_string_pretty(&self.meta)?;
        std::fs::write(&meta_path, content)?;
        Ok(())
    }

    /// Re-read meta.json from disk, picking up changes made by other processes (e.g. TUI)
    pub fn reload_meta(&mut self) -> Result<()> {
        let meta_path = self.dir.join("meta.json");
        let meta_content =
            std::fs::read_to_string(&meta_path).context("Failed to read task meta.json")?;
        self.meta =
            serde_json::from_str(&meta_content).context("Failed to parse task meta.json")?;
        Ok(())
    }

    pub fn update_status(&mut self, status: TaskStatus) -> Result<()> {
        tracing::debug!(task_id = %self.meta.task_id(), status = %status, "updating task status");
        self.meta.status = status;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    pub fn update_agent(&mut self, agent: Option<String>) -> Result<()> {
        self.meta.current_agent = agent;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    pub fn advance_flow_step(&mut self) -> Result<()> {
        self.meta.flow_step += 1;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    pub fn write_task(&self, content: &str) -> Result<()> {
        let task_path = self.meta.worktree_path.join("TASK.md");
        std::fs::write(&task_path, content)?;
        Ok(())
    }

    /// Ensure TASK.md and REVIEW.md are excluded from git tracking.
    ///
    /// Adds entries to .git/info/exclude (not tracked, leaves no footprint).
    /// For worktrees, uses the common git directory since they share info/exclude.
    fn ensure_git_excludes_task(&self) -> Result<()> {
        use std::process::Command;

        // Get the common git directory (main .git for worktrees, regular .git otherwise)
        // --git-common-dir returns the shared directory for worktrees
        let output = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(&self.meta.worktree_path)
            .output()
            .context("Failed to get git common directory")?;

        if !output.status.success() {
            anyhow::bail!("Failed to determine git common directory");
        }

        let git_common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let git_common_dir_path = if std::path::Path::new(&git_common_dir).is_absolute() {
            std::path::PathBuf::from(&git_common_dir)
        } else {
            self.meta.worktree_path.join(&git_common_dir)
        };

        let exclude_path = git_common_dir_path.join("info").join("exclude");
        let entries = ["TASK.md", "REVIEW.md"];

        // Ensure the info directory exists
        if let Some(info_dir) = exclude_path.parent() {
            std::fs::create_dir_all(info_dir).context("Failed to create .git/info directory")?;
        }

        let mut content = if exclude_path.exists() {
            std::fs::read_to_string(&exclude_path)
                .context("Failed to read .git/info/exclude")?
        } else {
            String::new()
        };

        let mut modified = false;
        for entry in &entries {
            let is_excluded = content.lines().any(|line| {
                let trimmed = line.trim();
                trimmed == *entry || trimmed == format!("/{}", entry)
            });

            if !is_excluded {
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str(entry);
                content.push('\n');
                modified = true;
            }
        }

        if modified {
            std::fs::write(&exclude_path, &content)
                .context("Failed to update .git/info/exclude")?;
        }

        Ok(())
    }

    fn init_files(&self) -> Result<()> {
        // Create empty files that will be populated later
        let files = [
            "progress.md",
            "compacted-context.md",
            "notes.md",
            "agent.log",
        ];

        for file in files {
            let path = self.dir.join(file);
            if !path.exists() {
                std::fs::write(&path, "")?;
            }
        }

        Ok(())
    }

    pub fn read_task(&self) -> Result<String> {
        let path = self.meta.worktree_path.join("TASK.md");
        std::fs::read_to_string(&path).context("Failed to read TASK.md")
    }

    pub fn read_progress(&self) -> Result<String> {
        let path = self.dir.join("progress.md");
        std::fs::read_to_string(&path).context("Failed to read progress.md")
    }

    pub fn read_context(&self) -> Result<String> {
        let path = self.dir.join("compacted-context.md");
        std::fs::read_to_string(&path).context("Failed to read compacted-context.md")
    }

    pub fn read_notes(&self) -> Result<String> {
        let path = self.dir.join("notes.md");
        std::fs::read_to_string(&path).context("Failed to read notes.md")
    }

    pub fn write_notes(&self, notes: &str) -> Result<()> {
        let path = self.dir.join("notes.md");
        std::fs::write(&path, notes)?;
        Ok(())
    }

    pub fn read_agent_log(&self) -> Result<String> {
        let path = self.dir.join("agent.log");
        std::fs::read_to_string(&path).context("Failed to read agent.log")
    }

    pub fn read_agent_log_tail(&self, lines: usize) -> Result<String> {
        let content = self.read_agent_log()?;
        let all_lines: Vec<&str> = content.lines().collect();
        let start = all_lines.len().saturating_sub(lines);
        Ok(all_lines[start..].join("\n"))
    }

    pub fn append_agent_log(&self, content: &str) -> Result<()> {
        use std::io::Write;
        let path = self.dir.join("agent.log");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", content)?;
        Ok(())
    }

    pub fn delete(self, config: &Config) -> Result<()> {
        tracing::info!(task_id = %self.meta.task_id(), "deleting task");
        let dir = config.task_dir(&self.meta.repo_name, &self.meta.branch_name);
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

    /// Write feedback for the refiner agent to process
    pub fn write_feedback(&self, feedback: &str) -> Result<()> {
        let path = self.dir.join("FEEDBACK.md");
        std::fs::write(&path, feedback)?;
        Ok(())
    }

    /// Read feedback (if any)
    pub fn read_feedback(&self) -> Result<String> {
        let path = self.dir.join("FEEDBACK.md");
        if path.exists() {
            std::fs::read_to_string(&path).context("Failed to read FEEDBACK.md")
        } else {
            Ok(String::new())
        }
    }

    /// Clear feedback after it's been processed
    pub fn clear_feedback(&self) -> Result<()> {
        let path = self.dir.join("FEEDBACK.md");
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Queue feedback to be processed when the task stops
    pub fn queue_feedback(&mut self, feedback: &str) -> Result<()> {
        tracing::debug!(task_id = %self.meta.task_id(), queue_size = self.meta.feedback_queue.len() + 1, "queuing feedback");
        self.meta.feedback_queue.push(feedback.to_string());
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    /// Pop the first feedback item from the queue
    pub fn pop_feedback_queue(&mut self) -> Result<Option<String>> {
        if self.meta.feedback_queue.is_empty() {
            return Ok(None);
        }
        let feedback = self.meta.feedback_queue.remove(0);
        self.meta.updated_at = Utc::now();
        self.save_meta()?;
        Ok(Some(feedback))
    }

    /// Check if there's queued feedback
    pub fn has_queued_feedback(&self) -> bool {
        !self.meta.feedback_queue.is_empty()
    }

    /// Get the number of queued feedback items
    pub fn queued_feedback_count(&self) -> usize {
        self.meta.feedback_queue.len()
    }

    /// Read all queued feedback items (for display purposes)
    pub fn read_feedback_queue(&self) -> &[String] {
        &self.meta.feedback_queue
    }

    /// Clear all queued feedback
    pub fn clear_feedback_queue(&mut self) -> Result<()> {
        self.meta.feedback_queue.clear();
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    /// Get the git diff for the worktree
    pub fn get_git_diff(&self) -> Result<String> {
        use std::process::Command;

        let output = Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(&self.meta.worktree_path)
            .output()
            .context("Failed to run git diff")?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Get a summary of commits on this branch
    pub fn get_git_log_summary(&self) -> Result<String> {
        use std::process::Command;

        // Try to get commits since branching from main/master
        let output = Command::new("git")
            .args(["log", "--oneline", "-20"])
            .current_dir(&self.meta.worktree_path)
            .output()
            .context("Failed to run git log")?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Reset flow step to 0 for re-running
    pub fn reset_flow_step(&mut self) -> Result<()> {
        self.meta.flow_step = 0;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }
}
