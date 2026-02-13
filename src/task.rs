use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone, Copy)]
enum SectionKind {
    AgentOutput,
    UserFeedback,
}

struct LogSection<'a> {
    kind: SectionKind,
    lines: Vec<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Stopped,
    InputNeeded,
    OnHold,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Stopped => write!(f, "stopped"),
            TaskStatus::InputNeeded => write!(f, "input needed"),
            TaskStatus::OnHold => write!(f, "on hold"),
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
    /// When true, run the review-pr command automatically after the flow completes
    #[serde(default)]
    pub review_after: bool,
    /// Linked GitHub PR (number + URL), populated when a PR is created
    #[serde(default)]
    pub linked_pr: Option<LinkedPr>,
    /// Number of reviews seen on the linked PR during last poll
    #[serde(default)]
    pub last_review_count: Option<u64>,
    /// Whether the address-review flow has been run since the last review
    #[serde(default)]
    pub review_addressed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPr {
    pub number: u64,
    pub url: String,
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
            review_after: false,
            linked_pr: None,
            last_review_count: None,
            review_addressed: false,
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
                        Ok(task) => tasks.push(task),
                        Err(e) => {
                            tracing::warn!(task_id = %task_id, error = %e, "failed to load task");
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
                TaskStatus::OnHold => 3,
            };
            let ord = order(a.meta.status).cmp(&order(b.meta.status));
            if ord != std::cmp::Ordering::Equal {
                ord
            } else {
                b.meta.updated_at.cmp(&a.meta.updated_at)
            }
        });

        tasks
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
    pub fn ensure_git_excludes_task(&self) -> Result<()> {
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

    /// Read a structured tail of agent.log that preserves section boundaries.
    ///
    /// Instead of a flat tail of N lines, this parses the log into sections
    /// (agent runs, user feedback, transitions) and returns a condensed view:
    /// - All agent start/finish markers are kept
    /// - All user feedback blocks are kept in full
    /// - All stop condition lines are kept
    /// - For each agent's output, only the last `per_agent_tail` lines are kept
    /// - Oldest agent sections are truncated first if total exceeds `max_lines`
    pub fn read_agent_log_structured_tail(&self, max_lines: usize) -> Result<String> {
        let content = self.read_agent_log()?;
        if content.is_empty() {
            return Ok(content);
        }

        let all_lines: Vec<&str> = content.lines().collect();

        // Parse into sections
        let mut sections: Vec<LogSection> = Vec::new();
        let mut current_lines: Vec<&str> = Vec::new();
        let mut current_kind = SectionKind::AgentOutput;

        for line in &all_lines {
            let trimmed = line.trim();

            if trimmed.starts_with("--- Agent:") && trimmed.ends_with("---") && trimmed.contains("started at") {
                // Flush previous section
                if !current_lines.is_empty() {
                    sections.push(LogSection { kind: current_kind, lines: std::mem::take(&mut current_lines) });
                }
                current_kind = SectionKind::AgentOutput;
                current_lines.push(line);
            } else if trimmed.starts_with("--- Agent:") && trimmed.ends_with("---") && trimmed.contains("finished at") {
                current_lines.push(line);
                sections.push(LogSection { kind: current_kind, lines: std::mem::take(&mut current_lines) });
                current_kind = SectionKind::AgentOutput;
            } else if trimmed.starts_with("--- User feedback at ") && trimmed.ends_with("---") {
                // Flush previous section
                if !current_lines.is_empty() {
                    sections.push(LogSection { kind: current_kind, lines: std::mem::take(&mut current_lines) });
                }
                current_kind = SectionKind::UserFeedback;
                current_lines.push(line);
            } else if trimmed == "--- End user feedback ---" {
                current_lines.push(line);
                sections.push(LogSection { kind: current_kind, lines: std::mem::take(&mut current_lines) });
                current_kind = SectionKind::AgentOutput;
            } else {
                current_lines.push(line);
            }
        }
        // Flush last section
        if !current_lines.is_empty() {
            sections.push(LogSection { kind: current_kind, lines: current_lines });
        }

        // Now condense: keep structural lines and tail of agent output
        let per_agent_tail = 30;
        let mut result_lines: Vec<String> = Vec::new();

        for section in &sections {
            match section.kind {
                SectionKind::UserFeedback => {
                    // Keep all feedback lines
                    for line in &section.lines {
                        result_lines.push(line.to_string());
                    }
                }
                SectionKind::AgentOutput => {
                    // Separate structural lines (markers, stop conditions) from normal output
                    let mut structural_head: Vec<&str> = Vec::new();
                    let mut output: Vec<&str> = Vec::new();
                    let mut structural_tail: Vec<&str> = Vec::new();

                    // The first line might be an agent start marker
                    let mut in_body = false;
                    for line in &section.lines {
                        let trimmed = line.trim();
                        let is_marker = (trimmed.starts_with("--- Agent:") && trimmed.ends_with("---"))
                            || trimmed.contains("AGENT_DONE")
                            || trimmed.contains("TASK_COMPLETE")
                            || trimmed.contains("TASK_BLOCKED")
                            || trimmed.contains("TESTS_PASS")
                            || trimmed.contains("TESTS_FAIL")
                            || trimmed.contains("INPUT_NEEDED");

                        if is_marker && !in_body {
                            structural_head.push(line);
                        } else if is_marker && in_body {
                            structural_tail.push(line);
                        } else {
                            in_body = true;
                            output.push(line);
                        }
                    }

                    // Always keep structural head
                    for line in &structural_head {
                        result_lines.push(line.to_string());
                    }

                    // Trim output if needed
                    if output.len() > per_agent_tail {
                        let trimmed_count = output.len() - per_agent_tail;
                        result_lines.push(format!("[... {} lines trimmed ...]", trimmed_count));
                        for line in &output[output.len() - per_agent_tail..] {
                            result_lines.push(line.to_string());
                        }
                    } else {
                        for line in &output {
                            result_lines.push(line.to_string());
                        }
                    }

                    // Always keep structural tail
                    for line in &structural_tail {
                        result_lines.push(line.to_string());
                    }
                }
            }
        }

        // Final truncation if still over max_lines: take the tail
        if result_lines.len() > max_lines {
            let start = result_lines.len() - max_lines;
            result_lines = result_lines.into_iter().skip(start).collect();
        }

        Ok(result_lines.join("\n"))
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

    /// Append a user feedback entry to agent.log with structured markers
    pub fn append_feedback_to_log(&self, feedback: &str) -> Result<()> {
        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let entry = format!(
            "\n--- User feedback at {} ---\n{}\n--- End user feedback ---\n",
            timestamp, feedback
        );
        self.append_agent_log(&entry)
    }

    /// Clear feedback after it's been processed
    pub fn clear_feedback(&self) -> Result<()> {
        let path = self.dir.join("FEEDBACK.md");
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Path to the separate feedback queue file (avoids meta.json write conflicts)
    fn queue_file_path(&self) -> PathBuf {
        self.dir.join("feedback_queue.json")
    }

    /// Read the feedback queue from its dedicated file
    fn read_queue_file(&self) -> Vec<String> {
        let path = self.queue_file_path();
        if !path.exists() {
            return Vec::new();
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Write the feedback queue to its dedicated file (deletes file if empty)
    fn write_queue_file(&self, queue: &[String]) -> Result<()> {
        let path = self.queue_file_path();
        if queue.is_empty() {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        } else {
            let content = serde_json::to_string_pretty(queue)?;
            std::fs::write(&path, content)?;
        }
        Ok(())
    }

    /// Queue feedback to be processed when the task stops
    pub fn queue_feedback(&self, feedback: &str) -> Result<()> {
        let mut queue = self.read_queue_file();
        tracing::debug!(task_id = %self.meta.task_id(), queue_size = queue.len() + 1, "queuing feedback");
        queue.push(feedback.to_string());
        self.write_queue_file(&queue)
    }

    /// Pop the first feedback item from the queue
    pub fn pop_feedback_queue(&self) -> Result<Option<String>> {
        let mut queue = self.read_queue_file();
        if queue.is_empty() {
            return Ok(None);
        }
        let feedback = queue.remove(0);
        self.write_queue_file(&queue)?;
        Ok(Some(feedback))
    }

    /// Check if there's queued feedback
    pub fn has_queued_feedback(&self) -> bool {
        !self.read_queue_file().is_empty()
    }

    /// Get the number of queued feedback items
    pub fn queued_feedback_count(&self) -> usize {
        self.read_queue_file().len()
    }

    /// Read all queued feedback items (for display purposes)
    pub fn read_feedback_queue(&self) -> Vec<String> {
        self.read_queue_file()
    }

    /// Remove a single feedback item by index
    pub fn remove_feedback_queue_item(&self, index: usize) -> Result<()> {
        let mut queue = self.read_queue_file();
        if index < queue.len() {
            queue.remove(index);
            self.write_queue_file(&queue)?;
        }
        Ok(())
    }

    /// Clear all queued feedback
    pub fn clear_feedback_queue(&self) -> Result<()> {
        let path = self.queue_file_path();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn set_linked_pr(&mut self, number: u64, url: String) -> Result<()> {
        self.meta.linked_pr = Some(LinkedPr { number, url });
        self.meta.updated_at = Utc::now();
        self.save_meta()
    }

    pub fn clear_linked_pr(&mut self) -> Result<()> {
        self.meta.linked_pr = None;
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
