use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::process::Command;
use std::time::{Duration, Instant};
use tui_textarea::{CursorMove, Input, TextArea};

use crate::command::StoredCommand;
use crate::config::Config;
use crate::git::Git;
use crate::task::{Task, TaskStatus};
use crate::tmux::Tmux;

use super::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    TaskList,
    Preview,
    DeleteConfirm,
    Feedback,
    NewTaskWizard,
    CommandList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    SelectRepo,
    SelectBranch,
    EnterDescription,
    SelectFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchMode {
    CreateNew,
    SelectExisting,
}

pub struct NewTaskWizard {
    pub step: WizardStep,
    pub repos: Vec<String>,
    pub selected_repo_index: usize,
    pub branch_mode: BranchMode,
    pub existing_branches: Vec<String>,
    pub selected_branch_index: usize,
    pub new_branch_editor: TextArea<'static>,
    pub description_editor: TextArea<'static>,
    pub flows: Vec<String>,
    pub selected_flow_index: usize,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPane {
    Logs,
    Notes,
}

pub struct App {
    pub config: Config,
    pub tasks: Vec<Task>,
    pub selected_index: usize,
    pub view: View,
    pub preview_content: String,
    pub preview_scroll: u16,
    pub notes_content: String,
    pub notes_scroll: u16,
    pub notes_editor: TextArea<'static>,
    pub notes_editing: bool,
    pub preview_pane: PreviewPane,
    pub feedback_editor: TextArea<'static>,
    pub should_quit: bool,
    pub status_message: Option<(String, Instant)>,
    pub wizard: Option<NewTaskWizard>,
    pub output_log: Vec<String>,
    pub output_scroll: u16,
    // Stored commands
    pub commands: Vec<StoredCommand>,
    pub selected_command_index: usize,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let tasks = Task::list_all(&config)?;
        let commands = StoredCommand::list_all(&config.commands_dir).unwrap_or_default();
        let notes_editor = Self::create_editor();
        let feedback_editor = Self::create_editor();

        Ok(Self {
            config,
            tasks,
            selected_index: 0,
            view: View::TaskList,
            preview_content: String::new(),
            preview_scroll: 0,
            notes_content: String::new(),
            notes_scroll: 0,
            notes_editor,
            notes_editing: false,
            preview_pane: PreviewPane::Logs,
            feedback_editor,
            should_quit: false,
            status_message: None,
            wizard: None,
            output_log: Vec::new(),
            output_scroll: 0,
            commands,
            selected_command_index: 0,
        })
    }

    fn create_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(ratatui::style::Style::default());
        editor
    }

    /// Auto-wrap text in a TextArea when lines exceed max_width.
    /// Finds the last word boundary and inserts a newline there.
    fn auto_wrap_editor(editor: &mut TextArea<'static>, max_width: usize) {
        if max_width < 20 {
            return;
        }

        let (row, col) = editor.cursor();
        let lines = editor.lines();

        if row >= lines.len() {
            return;
        }

        let current_line = &lines[row];
        if current_line.len() <= max_width {
            return;
        }

        // Find the last space before max_width
        let wrap_at = current_line[..max_width]
            .rfind(' ')
            .unwrap_or(max_width);

        if wrap_at == 0 {
            return;
        }

        // We need to split the line: move cursor to wrap point, insert newline
        // This is a bit tricky with tui-textarea's API
        // We'll rebuild the lines and recreate the editor content

        let mut new_lines: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i == row {
                // Split this line
                let (before, after) = line.split_at(wrap_at);
                new_lines.push(before.to_string());
                new_lines.push(after.trim_start().to_string());
            } else {
                new_lines.push(line.to_string());
            }
        }

        // Calculate new cursor position
        let new_col = if col > wrap_at {
            col - wrap_at - 1 // Account for removed space
        } else {
            col
        };
        let new_row = if col > wrap_at { row + 1 } else { row };

        // Recreate the editor with new content
        *editor = TextArea::from(new_lines.iter().map(|s| s.as_str()));
        editor.set_cursor_line_style(ratatui::style::Style::default());

        // Restore cursor position
        editor.move_cursor(CursorMove::Jump(new_row as u16, new_col as u16));
    }

    pub fn refresh_tasks(&mut self) -> Result<()> {
        self.tasks = Task::list_all(&self.config)?;
        if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_index = self.tasks.len() - 1;
        }
        Ok(())
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.selected_index)
    }

    pub fn set_status(&mut self, message: String) {
        self.status_message = Some((message, Instant::now()));
    }

    pub fn log_output(&mut self, message: String) {
        self.output_log.push(message);
        // Keep only the last 100 lines
        if self.output_log.len() > 100 {
            self.output_log.remove(0);
        }
        // Auto-scroll to bottom
        self.output_scroll = self.output_log.len().saturating_sub(5) as u16;
    }

    pub fn clear_output(&mut self) {
        self.output_log.clear();
        self.output_scroll = 0;
    }

    pub fn clear_old_status(&mut self) {
        if let Some((_, instant)) = &self.status_message {
            if instant.elapsed() > Duration::from_secs(3) {
                self.status_message = None;
            }
        }
    }

    fn next_task(&mut self) {
        if !self.tasks.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.tasks.len();
            self.preview_scroll = 0;
        }
    }

    fn previous_task(&mut self) {
        if !self.tasks.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.tasks.len() - 1
            } else {
                self.selected_index - 1
            };
            self.preview_scroll = 0;
        }
    }

    fn load_preview(&mut self) {
        let (preview_content, notes_content) = if let Some(task) = self.selected_task() {
            let preview = task
                .read_agent_log_tail(100)
                .unwrap_or_else(|_| "No agent log available".to_string());
            let notes = task.read_notes().unwrap_or_default();
            (preview, notes)
        } else {
            return;
        };

        self.preview_content = preview_content;
        // Scroll to bottom of logs (estimate based on line count)
        let line_count = self.preview_content.lines().count() as u16;
        self.preview_scroll = line_count.saturating_sub(20); // Leave ~20 lines visible
        self.notes_content = notes_content.clone();
        self.notes_scroll = 0;

        // Setup notes editor
        self.notes_editor = TextArea::from(notes_content.lines());
        self.notes_editor
            .set_cursor_line_style(ratatui::style::Style::default());
        // Move cursor to end of text
        self.notes_editor.move_cursor(CursorMove::Bottom);
        self.notes_editor.move_cursor(CursorMove::End);
        self.notes_editing = false;
    }

    fn save_notes(&mut self) -> Result<()> {
        if let Some(task) = self.selected_task() {
            let notes = self.notes_editor.lines().join("\n");
            task.write_notes(&notes)?;
            self.notes_content = notes;
            self.set_status("Notes saved".to_string());
        }
        Ok(())
    }

    fn start_task(&mut self) -> Result<()> {
        let task_info = self.selected_task().map(|t| {
            (t.meta.task_id(), t.meta.status == TaskStatus::Stopped)
        });

        if let Some((task_id, is_stopped)) = task_info {
            if !is_stopped {
                self.set_status(format!("Task already running: {}", task_id));
                return Ok(());
            }

            self.log_output(format!("Starting task {}...", task_id));

            // Run agman start - capture output to avoid corrupting TUI
            let output = Command::new("agman")
                .args(["start", &task_id])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    if !stdout.is_empty() {
                        for line in stdout.lines() {
                            self.log_output(line.to_string());
                        }
                    }
                    self.refresh_tasks()?;
                    self.set_status(format!("Started: {}", task_id));
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    self.log_output(format!("Failed to start: {}", stderr));
                    self.set_status("Failed to start task".to_string());
                }
                Err(e) => {
                    self.log_output(format!("Error: {}", e));
                    self.set_status(format!("Error: {}", e));
                }
            }
        }
        Ok(())
    }

    fn reset_task(&mut self) -> Result<()> {
        let task_info = self.selected_task().map(|t| {
            (t.meta.task_id(), t.meta.status == TaskStatus::Stopped)
        });

        if let Some((task_id, is_stopped)) = task_info {
            if !is_stopped {
                self.set_status(format!("Can only reset stopped tasks: {}", task_id));
                return Ok(());
            }

            self.log_output(format!("Resetting task {}...", task_id));

            // Run agman reset - capture output to avoid corrupting TUI
            let output = Command::new("agman")
                .args(["reset", &task_id])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    if !stdout.is_empty() {
                        for line in stdout.lines() {
                            self.log_output(line.to_string());
                        }
                    }
                    self.refresh_tasks()?;
                    self.set_status(format!("Reset: {}", task_id));
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    self.log_output(format!("Failed to reset: {}", stderr));
                    self.set_status("Failed to reset task".to_string());
                }
                Err(e) => {
                    self.log_output(format!("Error: {}", e));
                    self.set_status(format!("Error: {}", e));
                }
            }
        }
        Ok(())
    }

    fn delete_task(&mut self) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let task = self.tasks.remove(self.selected_index);
        let task_id = task.meta.task_id();
        let repo_name = task.meta.repo_name.clone();
        let branch_name = task.meta.branch_name.clone();
        let worktree_path = task.meta.worktree_path.clone();
        let tmux_session = task.meta.tmux_session.clone();

        self.log_output(format!("Deleting task {}...", task_id));

        // Kill tmux session
        let _ = Tmux::kill_session(&tmux_session);
        self.log_output("  Killed tmux session".to_string());

        // Remove worktree
        let repo_path = self.config.repo_path(&repo_name);
        let _ = Git::remove_worktree(&repo_path, &worktree_path);
        self.log_output("  Removed worktree".to_string());

        // Delete branch
        let _ = Git::delete_branch(&repo_path, &branch_name);
        self.log_output("  Deleted branch".to_string());

        // Delete task directory
        task.delete(&self.config)?;
        self.log_output("  Deleted task files".to_string());

        if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_index = self.tasks.len() - 1;
        }

        self.set_status(format!("Deleted: {}", task_id));
        self.view = View::TaskList;
        Ok(())
    }

    fn start_feedback(&mut self) {
        // Clear the feedback editor
        self.feedback_editor = Self::create_editor();
        self.view = View::Feedback;
    }

    fn submit_feedback(&mut self) -> Result<()> {
        let feedback = self.feedback_editor.lines().join("\n");
        if feedback.trim().is_empty() {
            self.set_status("Feedback cannot be empty".to_string());
            self.view = View::Preview;
            return Ok(());
        }

        let task_id = if let Some(task) = self.selected_task() {
            // Write feedback directly to the task's FEEDBACK.md file
            task.write_feedback(&feedback)?;
            task.meta.task_id()
        } else {
            self.set_status("No task selected".to_string());
            self.view = View::TaskList;
            return Ok(());
        };

        self.log_output(format!("Starting continue flow for {}...", task_id));

        // Run agman continue (reads feedback from FEEDBACK.md)
        // Capture output to avoid corrupting TUI
        let output = Command::new("agman")
            .args(["continue", &task_id, "--flow", "continue"])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stdout.is_empty() {
                    for line in stdout.lines() {
                        self.log_output(line.to_string());
                    }
                }
                if !stderr.is_empty() {
                    for line in stderr.lines() {
                        self.log_output(format!("[stderr] {}", line));
                    }
                }
                self.log_output(format!("Flow started for {}", task_id));
                self.set_status(format!("Feedback submitted for {}", task_id));
                self.refresh_tasks()?;
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                self.log_output(format!("Failed to start continue flow: {}", stderr));
                self.set_status("Failed to start continue flow".to_string());
            }
            Err(e) => {
                self.log_output(format!("Error: {}", e));
                self.set_status(format!("Error: {}", e));
            }
        }

        self.feedback_editor = Self::create_editor(); // Clear editor
        self.view = View::Preview;
        self.load_preview();
        Ok(())
    }

    // === Wizard Methods ===

    fn start_wizard(&mut self) -> Result<()> {
        let repos = self.scan_repos()?;
        let flows = self.scan_flows()?;

        if repos.is_empty() {
            self.set_status("No repositories found in ~/repos/".to_string());
            return Ok(());
        }

        let mut new_branch_editor = Self::create_editor();
        new_branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        let mut description_editor = Self::create_editor();
        description_editor.set_cursor_line_style(ratatui::style::Style::default());

        // Find index of "default" flow, or use 0
        let default_flow_index = flows.iter().position(|f| f == "default").unwrap_or(0);

        self.wizard = Some(NewTaskWizard {
            step: WizardStep::SelectRepo,
            repos,
            selected_repo_index: 0,
            branch_mode: BranchMode::CreateNew,
            existing_branches: Vec::new(),
            selected_branch_index: 0,
            new_branch_editor,
            description_editor,
            flows,
            selected_flow_index: default_flow_index,
            error_message: None,
        });

        self.view = View::NewTaskWizard;
        Ok(())
    }

    fn scan_repos(&self) -> Result<Vec<String>> {
        let repos_dir = &self.config.repos_dir;
        if !repos_dir.exists() {
            return Ok(Vec::new());
        }

        let mut repos = Vec::new();
        for entry in std::fs::read_dir(repos_dir)? {
            let entry = entry?;
            let path = entry.path();

            // Skip if not a directory
            if !path.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();

            // Skip worktree directories (ending with -wt)
            if name.ends_with("-wt") {
                continue;
            }

            // Check if it's a git repo
            if path.join(".git").exists() {
                repos.push(name);
            }
        }

        repos.sort();
        Ok(repos)
    }

    fn scan_flows(&self) -> Result<Vec<String>> {
        let flows_dir = &self.config.flows_dir;
        if !flows_dir.exists() {
            return Ok(Vec::new());
        }

        let mut flows = Vec::new();
        for entry in std::fs::read_dir(flows_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "yaml").unwrap_or(false) {
                if let Some(stem) = path.file_stem() {
                    flows.push(stem.to_string_lossy().to_string());
                }
            }
        }

        flows.sort();
        Ok(flows)
    }

    pub fn scan_commands(&mut self) -> Result<()> {
        self.commands = StoredCommand::list_all(&self.config.commands_dir)?;
        if self.selected_command_index >= self.commands.len() && !self.commands.is_empty() {
            self.selected_command_index = self.commands.len() - 1;
        }
        Ok(())
    }

    fn open_command_list(&mut self) {
        if self.tasks.is_empty() {
            self.set_status("No task selected to run command on".to_string());
            return;
        }
        // Refresh commands list
        let _ = self.scan_commands();
        if self.commands.is_empty() {
            self.set_status("No stored commands available".to_string());
            return;
        }
        self.selected_command_index = 0;
        self.view = View::CommandList;
    }

    fn run_selected_command(&mut self) -> Result<()> {
        let task_id = match self.selected_task() {
            Some(t) => t.meta.task_id(),
            None => {
                self.set_status("No task selected".to_string());
                self.view = View::TaskList;
                return Ok(());
            }
        };

        let command = match self.commands.get(self.selected_command_index) {
            Some(c) => c.clone(),
            None => {
                self.set_status("No command selected".to_string());
                self.view = View::TaskList;
                return Ok(());
            }
        };

        self.log_output(format!("Running command '{}' on task {}...", command.name, task_id));

        // Run agman run-command in background
        let output = Command::new("agman")
            .args(["run-command", &task_id, &command.id])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if !stdout.is_empty() {
                    for line in stdout.lines() {
                        self.log_output(line.to_string());
                    }
                }
                self.refresh_tasks()?;
                self.set_status(format!("Started: {}", command.name));
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                self.log_output(format!("Failed: {}", stderr));
                self.set_status(format!("Failed to run {}", command.name));
            }
            Err(e) => {
                self.log_output(format!("Error: {}", e));
                self.set_status(format!("Error: {}", e));
            }
        }

        self.view = View::TaskList;
        Ok(())
    }

    fn scan_branches(&self, repo_name: &str) -> Result<Vec<String>> {
        let repo_path = self.config.repo_path(repo_name);

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "--list", "--format=%(refname:short)"])
            .output()?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut branches: Vec<String> = stdout.lines().map(|s| s.to_string()).collect();

        // Filter out branches that already have tasks
        let existing_tasks: std::collections::HashSet<String> = self
            .tasks
            .iter()
            .filter(|t| t.meta.repo_name == repo_name)
            .map(|t| t.meta.branch_name.clone())
            .collect();

        branches.retain(|b| !existing_tasks.contains(b));
        branches.sort();

        Ok(branches)
    }

    fn wizard_next_step(&mut self) -> Result<()> {
        let wizard = match &mut self.wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        wizard.error_message = None;

        match wizard.step {
            WizardStep::SelectRepo => {
                // Load branches for selected repo
                let repo_name = wizard.repos[wizard.selected_repo_index].clone();
                let branches = self.scan_branches(&repo_name)?;

                let wizard = self.wizard.as_mut().unwrap();
                wizard.existing_branches = branches;
                wizard.selected_branch_index = 0;
                wizard.step = WizardStep::SelectBranch;
            }
            WizardStep::SelectBranch => {
                // Validate branch name
                let branch_name = match wizard.branch_mode {
                    BranchMode::CreateNew => {
                        let name = wizard.new_branch_editor.lines().join("");
                        let name = name.trim().to_string();
                        if name.is_empty() {
                            wizard.error_message = Some("Branch name cannot be empty".to_string());
                            return Ok(());
                        }
                        // Check for invalid characters
                        if name.contains(' ')
                            || name.contains("..")
                            || name.starts_with('/')
                            || name.ends_with('/')
                        {
                            wizard.error_message =
                                Some("Invalid branch name format".to_string());
                            return Ok(());
                        }
                        name
                    }
                    BranchMode::SelectExisting => {
                        if wizard.existing_branches.is_empty() {
                            wizard.error_message =
                                Some("No existing branches available".to_string());
                            return Ok(());
                        }
                        wizard.existing_branches[wizard.selected_branch_index].clone()
                    }
                };

                // Check if task already exists
                let repo_name = &wizard.repos[wizard.selected_repo_index];
                let task_dir = self.config.task_dir(repo_name, &branch_name);
                if task_dir.exists() {
                    wizard.error_message = Some(format!(
                        "Task '{}--{}' already exists",
                        repo_name, branch_name
                    ));
                    return Ok(());
                }

                wizard.step = WizardStep::EnterDescription;
            }
            WizardStep::EnterDescription => {
                let description = wizard.description_editor.lines().join("\n");
                let description = description.trim();
                if description.is_empty() {
                    wizard.error_message = Some("Description cannot be empty".to_string());
                    return Ok(());
                }
                wizard.step = WizardStep::SelectFlow;
            }
            WizardStep::SelectFlow => {
                // Create the task
                return self.create_task_from_wizard();
            }
        }

        Ok(())
    }

    fn wizard_prev_step(&mut self) {
        let wizard = match &mut self.wizard {
            Some(w) => w,
            None => return,
        };

        wizard.error_message = None;

        match wizard.step {
            WizardStep::SelectRepo => {
                // Cancel wizard
                self.wizard = None;
                self.view = View::TaskList;
            }
            WizardStep::SelectBranch => {
                wizard.step = WizardStep::SelectRepo;
            }
            WizardStep::EnterDescription => {
                wizard.step = WizardStep::SelectBranch;
            }
            WizardStep::SelectFlow => {
                wizard.step = WizardStep::EnterDescription;
            }
        }
    }

    fn create_task_from_wizard(&mut self) -> Result<()> {
        let wizard = match &self.wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        let repo_name = wizard.repos[wizard.selected_repo_index].clone();
        let branch_name = match wizard.branch_mode {
            BranchMode::CreateNew => wizard.new_branch_editor.lines().join("").trim().to_string(),
            BranchMode::SelectExisting => {
                wizard.existing_branches[wizard.selected_branch_index].clone()
            }
        };
        let description = wizard.description_editor.lines().join("\n").trim().to_string();
        let flow_name = wizard.flows[wizard.selected_flow_index].clone();

        self.log_output(format!("Creating task {}--{}...", repo_name, branch_name));

        // Initialize default files
        self.config.init_default_files()?;

        // Create worktree (use quiet mode to avoid corrupting TUI)
        self.log_output("  Creating worktree...".to_string());
        let worktree_path = match Git::create_worktree_quiet(&self.config, &repo_name, &branch_name) {
            Ok(path) => path,
            Err(e) => {
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to create worktree: {}", e));
                }
                return Ok(());
            }
        };

        // Run direnv allow
        let _ = Git::direnv_allow(&worktree_path);

        // Create task
        self.log_output("  Creating task files...".to_string());
        let task = match Task::create(
            &self.config,
            &repo_name,
            &branch_name,
            &description,
            &flow_name,
            worktree_path.clone(),
        ) {
            Ok(t) => t,
            Err(e) => {
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to create task: {}", e));
                }
                return Ok(());
            }
        };

        // Create tmux session with windows
        self.log_output("  Creating tmux session...".to_string());
        if let Err(e) =
            Tmux::create_session_with_windows(&task.meta.tmux_session, &worktree_path)
        {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        // Start the flow in tmux
        let task_id = task.meta.task_id();
        let flow_cmd = format!("agman flow-run {}", task_id);
        let _ = Tmux::send_keys_to_window(&task.meta.tmux_session, "agman", &flow_cmd);
        self.log_output(format!("  Started flow: {}", flow_name));

        // Success - close wizard and refresh
        self.wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks()?;
        self.set_status(format!("Created task: {}", task_id));

        Ok(())
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool> {
        self.clear_old_status();

        match self.view {
            View::TaskList => self.handle_task_list_event(event),
            View::Preview => self.handle_preview_event(event),
            View::DeleteConfirm => self.handle_delete_confirm_event(event),
            View::Feedback => self.handle_feedback_event(event),
            View::NewTaskWizard => self.handle_wizard_event(event),
            View::CommandList => self.handle_command_list_event(event),
        }
    }

    fn handle_task_list_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.should_quit = true;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Char('k') => {
                    self.previous_task();
                }
                KeyCode::Char('j') => {
                    self.next_task();
                }
                KeyCode::Enter | KeyCode::Char('l') => {
                    self.load_preview();
                    self.preview_pane = PreviewPane::Logs;
                    self.view = View::Preview;
                }
                KeyCode::Char('s') => {
                    self.start_task()?;
                }
                KeyCode::Char('r') => {
                    self.reset_task()?;
                }
                KeyCode::Char('d') => {
                    if !self.tasks.is_empty() {
                        self.view = View::DeleteConfirm;
                    }
                }
                KeyCode::Char('R') => {
                    self.refresh_tasks()?;
                    self.set_status("Refreshed task list".to_string());
                }
                KeyCode::Char('f') => {
                    if !self.tasks.is_empty() {
                        self.load_preview();
                        self.start_feedback();
                    }
                }
                KeyCode::Char('G') => {
                    // Jump to last task
                    if !self.tasks.is_empty() {
                        self.selected_index = self.tasks.len() - 1;
                    }
                }
                KeyCode::Char('g') => {
                    // Jump to first task (gg in vim, but single g here)
                    self.selected_index = 0;
                }
                KeyCode::Char('n') => {
                    // Start new task wizard
                    self.start_wizard()?;
                }
                KeyCode::Char('c') => {
                    // Clear output log
                    self.clear_output();
                }
                KeyCode::Char('x') => {
                    // Open command list
                    self.open_command_list();
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_preview_event(&mut self, event: Event) -> Result<bool> {
        // If editing notes, handle vim-style input
        if self.notes_editing {
            return self.handle_notes_editing(event);
        }

        if let Event::Key(key) = event {
            // Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if key.code == KeyCode::Char('c') {
                    self.should_quit = true;
                    return Ok(false);
                }
            }

            // Tab to switch panes
            if key.code == KeyCode::Tab {
                self.preview_pane = match self.preview_pane {
                    PreviewPane::Logs => PreviewPane::Notes,
                    PreviewPane::Notes => PreviewPane::Logs,
                };
                return Ok(false);
            }

            // BackTab (Shift+Tab) to switch panes in reverse
            if key.code == KeyCode::BackTab {
                self.preview_pane = match self.preview_pane {
                    PreviewPane::Logs => PreviewPane::Notes,
                    PreviewPane::Notes => PreviewPane::Logs,
                };
                return Ok(false);
            }

            // Shift+J/K for scrolling
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::Char('J') => {
                        match self.preview_pane {
                            PreviewPane::Logs => {
                                self.preview_scroll = self.preview_scroll.saturating_add(3);
                            }
                            PreviewPane::Notes => {
                                self.notes_scroll = self.notes_scroll.saturating_add(3);
                            }
                        }
                        return Ok(false);
                    }
                    KeyCode::Char('K') => {
                        match self.preview_pane {
                            PreviewPane::Logs => {
                                self.preview_scroll = self.preview_scroll.saturating_sub(3);
                            }
                            PreviewPane::Notes => {
                                self.notes_scroll = self.notes_scroll.saturating_sub(3);
                            }
                        }
                        return Ok(false);
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => {
                    self.view = View::TaskList;
                }
                KeyCode::Enter => {
                    // In Notes pane, Enter starts editing; in Logs, attaches tmux
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            if let Some(task) = self.selected_task() {
                                if Tmux::session_exists(&task.meta.tmux_session) {
                                    return Ok(true);
                                }
                            }
                        }
                        PreviewPane::Notes => {
                            self.notes_editing = true;
                            self.set_status("Editing notes (Esc to save & exit)".to_string());
                        }
                    }
                }
                KeyCode::Char('i') => {
                    // Enter edit mode for notes
                    if self.preview_pane == PreviewPane::Notes {
                        self.notes_editing = true;
                        self.set_status("Editing notes (Esc to save & exit)".to_string());
                    }
                }
                KeyCode::Char('f') => {
                    // Give feedback
                    self.start_feedback();
                }
                KeyCode::Char('x') => {
                    // Open command list
                    self.open_command_list();
                }
                KeyCode::Char('j') => {
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = self.preview_scroll.saturating_add(1);
                        }
                        PreviewPane::Notes => {
                            self.notes_scroll = self.notes_scroll.saturating_add(1);
                        }
                    }
                }
                KeyCode::Char('k') => {
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = self.preview_scroll.saturating_sub(1);
                        }
                        PreviewPane::Notes => {
                            self.notes_scroll = self.notes_scroll.saturating_sub(1);
                        }
                    }
                }
                KeyCode::Char('l') => {
                    self.preview_pane = PreviewPane::Notes;
                }
                KeyCode::Char('G') => {
                    // Jump to bottom
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = u16::MAX / 2;
                        }
                        PreviewPane::Notes => {
                            self.notes_scroll = u16::MAX / 2;
                        }
                    }
                }
                KeyCode::Char('g') => {
                    // Jump to top
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = 0;
                        }
                        PreviewPane::Notes => {
                            self.notes_scroll = 0;
                        }
                    }
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Half page down
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = self.preview_scroll.saturating_add(15);
                        }
                        PreviewPane::Notes => {
                            self.notes_scroll = self.notes_scroll.saturating_add(15);
                        }
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Half page up
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = self.preview_scroll.saturating_sub(15);
                        }
                        PreviewPane::Notes => {
                            self.notes_scroll = self.notes_scroll.saturating_sub(15);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_notes_editing(&mut self, event: Event) -> Result<bool> {
        // Calculate wrap width dynamically: notes panel is 40% of screen width, minus borders
        let wrap_width = crossterm::terminal::size()
            .map(|(w, _)| ((w as f32 * 0.40) as usize).saturating_sub(4))
            .unwrap_or(40);

        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Esc => {
                    self.notes_editing = false;
                    self.save_notes()?;
                }
                _ => {
                    // Convert crossterm event to tui-textarea Input
                    let input = Input::from(event.clone());
                    self.notes_editor.input(input);
                    // Auto-wrap long lines
                    Self::auto_wrap_editor(&mut self.notes_editor, wrap_width);
                }
            }
        }
        Ok(false)
    }

    fn handle_feedback_event(&mut self, event: Event) -> Result<bool> {
        // Calculate wrap width dynamically: feedback modal is 70% of screen width, minus borders
        let wrap_width = crossterm::terminal::size()
            .map(|(w, _)| ((w as f32 * 0.70) as usize).saturating_sub(6))
            .unwrap_or(70);

        if let Event::Key(key) = event {
            // Check for Ctrl+S to submit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.submit_feedback()?;
                return Ok(false);
            }

            match key.code {
                KeyCode::Esc => {
                    // Cancel feedback
                    self.view = View::Preview;
                    self.set_status("Feedback cancelled".to_string());
                }
                _ => {
                    let input = Input::from(event.clone());
                    self.feedback_editor.input(input);
                    // Auto-wrap long lines
                    Self::auto_wrap_editor(&mut self.feedback_editor, wrap_width);
                }
            }
        }
        Ok(false)
    }

    fn handle_delete_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.delete_task()?;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.view = View::TaskList;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_wizard_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            let wizard = match &mut self.wizard {
                Some(w) => w,
                None => {
                    self.view = View::TaskList;
                    return Ok(false);
                }
            };

            // Clear error on any keypress
            wizard.error_message = None;

            match wizard.step {
                WizardStep::SelectRepo => {
                    match key.code {
                        KeyCode::Esc => {
                            self.wizard = None;
                            self.view = View::TaskList;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !wizard.repos.is_empty() {
                                wizard.selected_repo_index =
                                    (wizard.selected_repo_index + 1) % wizard.repos.len();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if !wizard.repos.is_empty() {
                                wizard.selected_repo_index = if wizard.selected_repo_index == 0 {
                                    wizard.repos.len() - 1
                                } else {
                                    wizard.selected_repo_index - 1
                                };
                            }
                        }
                        KeyCode::Enter => {
                            self.wizard_next_step()?;
                        }
                        _ => {}
                    }
                }
                WizardStep::SelectBranch => {
                    match key.code {
                        KeyCode::Esc => {
                            self.wizard_prev_step();
                        }
                        KeyCode::Tab | KeyCode::BackTab => {
                            // Toggle between CreateNew and SelectExisting
                            wizard.branch_mode = match wizard.branch_mode {
                                BranchMode::CreateNew => BranchMode::SelectExisting,
                                BranchMode::SelectExisting => BranchMode::CreateNew,
                            };
                        }
                        KeyCode::Enter => {
                            self.wizard_next_step()?;
                        }
                        _ => {
                            match wizard.branch_mode {
                                BranchMode::CreateNew => {
                                    // Handle text input
                                    match key.code {
                                        KeyCode::Char('j') | KeyCode::Char('k')
                                            if !key
                                                .modifiers
                                                .contains(KeyModifiers::CONTROL) =>
                                        {
                                            // Pass j/k as text input in create mode
                                            let input = Input::from(event.clone());
                                            wizard.new_branch_editor.input(input);
                                        }
                                        _ => {
                                            let input = Input::from(event.clone());
                                            wizard.new_branch_editor.input(input);
                                        }
                                    }
                                }
                                BranchMode::SelectExisting => {
                                    // Handle list navigation
                                    match key.code {
                                        KeyCode::Char('j') | KeyCode::Down => {
                                            if !wizard.existing_branches.is_empty() {
                                                wizard.selected_branch_index =
                                                    (wizard.selected_branch_index + 1)
                                                        % wizard.existing_branches.len();
                                            }
                                        }
                                        KeyCode::Char('k') | KeyCode::Up => {
                                            if !wizard.existing_branches.is_empty() {
                                                wizard.selected_branch_index =
                                                    if wizard.selected_branch_index == 0 {
                                                        wizard.existing_branches.len() - 1
                                                    } else {
                                                        wizard.selected_branch_index - 1
                                                    };
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
                WizardStep::EnterDescription => {
                    // Calculate wrap width dynamically: wizard is 80% of screen width, minus borders
                    let wrap_width = crossterm::terminal::size()
                        .map(|(w, _)| ((w as f32 * 0.80) as usize).saturating_sub(6))
                        .unwrap_or(70);

                    // Check for Ctrl+S to submit
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('s')
                    {
                        self.wizard_next_step()?;
                        return Ok(false);
                    }

                    match key.code {
                        KeyCode::Esc => {
                            self.wizard_prev_step();
                        }
                        _ => {
                            let input = Input::from(event.clone());
                            wizard.description_editor.input(input);
                            // Auto-wrap long lines
                            Self::auto_wrap_editor(&mut wizard.description_editor, wrap_width);
                        }
                    }
                }
                WizardStep::SelectFlow => {
                    match key.code {
                        KeyCode::Esc => {
                            self.wizard_prev_step();
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !wizard.flows.is_empty() {
                                wizard.selected_flow_index =
                                    (wizard.selected_flow_index + 1) % wizard.flows.len();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if !wizard.flows.is_empty() {
                                wizard.selected_flow_index = if wizard.selected_flow_index == 0 {
                                    wizard.flows.len() - 1
                                } else {
                                    wizard.selected_flow_index - 1
                                };
                            }
                        }
                        KeyCode::Enter => {
                            self.wizard_next_step()?;
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(false)
    }

    fn handle_command_list_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.commands.is_empty() {
                        self.selected_command_index =
                            (self.selected_command_index + 1) % self.commands.len();
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !self.commands.is_empty() {
                        self.selected_command_index = if self.selected_command_index == 0 {
                            self.commands.len() - 1
                        } else {
                            self.selected_command_index - 1
                        };
                    }
                }
                KeyCode::Enter => {
                    self.run_selected_command()?;
                }
                _ => {}
            }
        }
        Ok(false)
    }
}

pub fn run_tui(config: Config) -> Result<()> {
    // Create app once (persists across attach/return cycles)
    let mut app = App::new(config)?;

    loop {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Reset view state when returning from attach
        app.view = View::TaskList;
        app.should_quit = false;
        let _ = app.refresh_tasks();

        // Main loop
        let mut attach_session: Option<String> = None;
        let mut last_refresh = Instant::now();
        let refresh_interval = Duration::from_secs(3);

        loop {
            terminal.draw(|f| ui::draw(f, &mut app))?;

            if event::poll(Duration::from_millis(250))? {
                let event = event::read()?;
                let should_attach = app.handle_event(event)?;

                if should_attach {
                    if let Some(task) = app.selected_task() {
                        attach_session = Some(task.meta.tmux_session.clone());
                    }
                    break;
                }

                if app.should_quit {
                    break;
                }
            }

            // Periodic refresh of task list (every 3 seconds, only in TaskList view)
            if last_refresh.elapsed() >= refresh_interval {
                if app.view == View::TaskList {
                    let _ = app.refresh_tasks();
                }
                last_refresh = Instant::now();
            }

            // Clear old status messages
            app.clear_old_status();
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        // If user requested quit, exit the outer loop
        if app.should_quit {
            break;
        }

        // Attach to tmux if requested, then loop back to restart TUI
        if let Some(session) = attach_session {
            Tmux::attach_session(&session)?;
            // After detaching or switching back, the loop continues and TUI restarts
        }
    }

    Ok(())
}
