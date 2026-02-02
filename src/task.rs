use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Working,
    Paused,
    Done,
    Failed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Working => write!(f, "working"),
            TaskStatus::Paused => write!(f, "paused"),
            TaskStatus::Done => write!(f, "done"),
            TaskStatus::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    pub branch_name: String,
    pub status: TaskStatus,
    pub tmux_session: String,
    pub worktree_path: PathBuf,
    pub flow_name: String,
    pub current_agent: Option<String>,
    pub flow_step: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TaskMeta {
    pub fn new(branch_name: String, worktree_path: PathBuf, flow_name: String) -> Self {
        let now = Utc::now();
        Self {
            tmux_session: format!("agman-{}", branch_name),
            branch_name,
            status: TaskStatus::Working,
            worktree_path,
            flow_name,
            current_agent: None,
            flow_step: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug)]
pub struct Task {
    pub meta: TaskMeta,
    pub dir: PathBuf,
}

impl Task {
    pub fn create(config: &Config, branch_name: &str, description: &str, flow_name: &str, worktree_path: PathBuf) -> Result<Self> {
        let dir = config.task_dir(branch_name);
        std::fs::create_dir_all(&dir).context("Failed to create task directory")?;

        let meta = TaskMeta::new(branch_name.to_string(), worktree_path, flow_name.to_string());

        let task = Self { meta, dir };
        task.save_meta()?;
        task.write_prompt(description)?;
        task.init_files()?;

        Ok(task)
    }

    pub fn load(config: &Config, branch_name: &str) -> Result<Self> {
        let dir = config.task_dir(branch_name);
        if !dir.exists() {
            anyhow::bail!("Task '{}' does not exist", branch_name);
        }

        let meta_path = dir.join("meta.json");
        let meta_content = std::fs::read_to_string(&meta_path)
            .context("Failed to read task meta.json")?;
        let meta: TaskMeta = serde_json::from_str(&meta_content)
            .context("Failed to parse task meta.json")?;

        Ok(Self { meta, dir })
    }

    pub fn list_all(config: &Config) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if !config.tasks_dir.exists() {
            return Ok(tasks);
        }

        for entry in std::fs::read_dir(&config.tasks_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let branch_name = entry.file_name().to_string_lossy().to_string();
                match Task::load(config, &branch_name) {
                    Ok(task) => tasks.push(task),
                    Err(e) => {
                        tracing::warn!("Failed to load task '{}': {}", branch_name, e);
                    }
                }
            }
        }

        // Sort by updated_at descending
        tasks.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));

        Ok(tasks)
    }

    pub fn save_meta(&self) -> Result<()> {
        let meta_path = self.dir.join("meta.json");
        let content = serde_json::to_string_pretty(&self.meta)?;
        std::fs::write(&meta_path, content)?;
        Ok(())
    }

    pub fn update_status(&mut self, status: TaskStatus) -> Result<()> {
        self.meta.status = status;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    pub fn update_agent(&mut self, agent: Option<String>) -> Result<()> {
        self.meta.current_agent = agent;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    #[allow(dead_code)]
    pub fn advance_flow_step(&mut self) -> Result<()> {
        self.meta.flow_step += 1;
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    fn write_prompt(&self, description: &str) -> Result<()> {
        let prompt_path = self.dir.join("PROMPT.md");
        std::fs::write(&prompt_path, description)?;
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

    pub fn read_prompt(&self) -> Result<String> {
        let path = self.dir.join("PROMPT.md");
        std::fs::read_to_string(&path).context("Failed to read PROMPT.md")
    }

    pub fn read_plan(&self) -> Result<String> {
        let path = self.dir.join("PLAN.md");
        if path.exists() {
            std::fs::read_to_string(&path).context("Failed to read PLAN.md")
        } else {
            Ok(String::new())
        }
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
        let dir = config.task_dir(&self.meta.branch_name);
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
}
