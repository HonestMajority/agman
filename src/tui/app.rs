use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, widgets::ListState, Terminal};
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;
use tokio::sync::mpsc as tokio_mpsc;
use std::time::{Duration, Instant};
use tui_textarea::{CursorMove, Input, Key, TextArea};

use agman::command::StoredCommand;
use agman::config::Config;
use agman::flow::Flow;
use agman::git::Git;
use agman::repo_stats::RepoStats;
use agman::task::{Task, TaskStatus};
use agman::tmux::Tmux;
use agman::use_cases;

use super::ui;
use super::vim::{VimMode, VimTextArea};

/// Open a URL in the default browser, cross-platform (macOS / Linux).
fn open_url(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(cmd).arg(url).spawn();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    TaskList,
    Preview,
    DeleteConfirm,
    Feedback,
    NewTaskWizard,
    CommandList,
    TaskEditor,
    FeedbackQueue,
    RebaseBranchPicker,
    ReviewWizard,
    RestartConfirm,
    RestartWizard,
    SetLinkedPr,
    DirectoryPicker,
}

/// Which wizard requested the directory picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirPickerOrigin {
    NewTask,
    Review,
}

pub struct DirectoryPicker {
    pub current_dir: PathBuf,
    pub entries: Vec<String>,
    pub selected_index: usize,
    pub origin: DirPickerOrigin,
}

impl DirectoryPicker {
    fn new(start_dir: PathBuf, origin: DirPickerOrigin) -> Self {
        let mut picker = Self {
            current_dir: start_dir,
            entries: Vec::new(),
            selected_index: 0,
            origin,
        };
        picker.refresh_entries();
        picker
    }

    fn refresh_entries(&mut self) {
        self.entries.clear();
        if let Ok(read_dir) = std::fs::read_dir(&self.current_dir) {
            let mut dirs: Vec<String> = read_dir
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter(|e| {
                    !e.file_name()
                        .to_string_lossy()
                        .starts_with('.')
                })
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            dirs.sort();
            self.entries = dirs;
        }
        self.selected_index = 0;
    }

    fn enter_selected(&mut self) {
        if let Some(name) = self.entries.get(self.selected_index) {
            self.current_dir = self.current_dir.join(name);
            self.refresh_entries();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.refresh_entries();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    SelectRepo,
    SelectBranch,
    EnterDescription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchSource {
    NewBranch,
    ExistingBranch,
    ExistingWorktree,
}

pub struct NewTaskWizard {
    pub step: WizardStep,
    pub repos: Vec<String>,
    pub favorite_repos: Vec<(String, u64)>,
    pub selected_repo_index: usize,
    pub branch_source: BranchSource,
    pub existing_worktrees: Vec<(String, PathBuf)>,
    pub selected_worktree_index: usize,
    pub existing_branches: Vec<String>,
    pub selected_branch_index: usize,
    pub new_branch_editor: TextArea<'static>,
    pub description_editor: VimTextArea<'static>,
    pub error_message: Option<String>,
    pub review_after: bool,
}

impl NewTaskWizard {
    /// Total number of selectable items (favorites + all repos)
    pub fn total_repo_count(&self) -> usize {
        self.favorite_repos.len() + self.repos.len()
    }

    /// Resolve the current selection to a repo name
    pub fn selected_repo_name(&self) -> &str {
        if self.selected_repo_index < self.favorite_repos.len() {
            &self.favorite_repos[self.selected_repo_index].0
        } else {
            &self.repos[self.selected_repo_index - self.favorite_repos.len()]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewWizardStep {
    SelectRepo,
    EnterBranch,
}

pub struct ReviewWizard {
    pub step: ReviewWizardStep,
    pub repos: Vec<String>,
    pub favorite_repos: Vec<(String, u64)>,
    pub selected_repo_index: usize,
    pub branch_source: BranchSource,
    pub branch_editor: TextArea<'static>,
    pub existing_branches: Vec<String>,
    pub selected_branch_index: usize,
    pub existing_worktrees: Vec<(String, PathBuf)>,
    pub selected_worktree_index: usize,
    pub error_message: Option<String>,
}

impl ReviewWizard {
    pub fn total_repo_count(&self) -> usize {
        self.favorite_repos.len() + self.repos.len()
    }

    pub fn selected_repo_name(&self) -> &str {
        if self.selected_repo_index < self.favorite_repos.len() {
            &self.favorite_repos[self.selected_repo_index].0
        } else {
            &self.repos[self.selected_repo_index - self.favorite_repos.len()]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartWizardStep {
    EditTask,
    SelectAgent,
}

pub struct RestartWizard {
    pub step: RestartWizardStep,
    pub task_editor: VimTextArea<'static>,
    pub flow_steps: Vec<String>,
    pub selected_step_index: usize,
    pub task_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    Everything,
    TaskOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPane {
    Logs,
    Notes,
}

struct PrPollResult {
    task_id: String,
    pr_number: u64,
    action: use_cases::PrPollAction,
    review_count: u64,
}

/// Query a PR's state via `gh pr view`. Returns `(is_merged, review_count)`.
/// Returns `None` on any error so polling gracefully skips failures.
fn query_pr_state(worktree_path: &std::path::Path, pr_number: u64) -> Option<(bool, u64)> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "state,reviews",
        ])
        .current_dir(worktree_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let state = json.get("state")?.as_str()?;
    let is_merged = state == "MERGED";
    let review_count = json
        .get("reviews")
        .and_then(|r| r.as_array())
        .map(|a| a.len() as u64)
        .unwrap_or(0);

    Some((is_merged, review_count))
}

/// Look up the PR number for a branch using `gh pr list`.
/// Returns `None` if no PR is found or on any error.
fn lookup_pr_for_branch(worktree_path: &std::path::Path, branch_name: &str) -> Option<u64> {
    let output = Command::new("gh")
        .args([
            "pr", "list",
            "--head", branch_name,
            "--json", "number",
            "--limit", "1",
        ])
        .current_dir(worktree_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let arr = json.as_array()?;
    let first = arr.first()?;
    first.get("number")?.as_u64()
}

/// Run PR queries for all eligible tasks. This is the blocking work that
/// runs on a background thread — calls `gh pr view` for each task.
fn run_pr_queries(eligible: Vec<(String, u64, PathBuf, Option<u64>)>) -> Vec<PrPollResult> {
    let mut results = Vec::new();
    for (task_id, pr_number, worktree_path, last_review_count) in &eligible {
        if let Some((is_merged, review_count)) = query_pr_state(worktree_path, *pr_number) {
            let action = use_cases::determine_pr_poll_action(
                TaskStatus::Stopped,
                is_merged,
                review_count,
                *last_review_count,
            );
            results.push(PrPollResult {
                task_id: task_id.clone(),
                pr_number: *pr_number,
                action,
                review_count,
            });
        }
    }
    results
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
    pub notes_editor: VimTextArea<'static>,
    pub notes_editing: bool,
    pub preview_pane: PreviewPane,
    pub feedback_editor: VimTextArea<'static>,
    pub should_quit: bool,
    pub status_message: Option<(String, Instant)>,
    pub wizard: Option<NewTaskWizard>,
    pub review_wizard: Option<ReviewWizard>,
    pub output_log: Vec<String>,
    pub output_scroll: u16,
    pub last_output_time: Option<Instant>,
    // Task file (TASK.md) viewing/editing (used by modal)
    pub task_file_content: String,
    pub task_file_editor: VimTextArea<'static>,
    /// When true, saving the task editor will auto-resume the flow (used for answering questions)
    pub answering_questions: bool,
    // Stored commands
    pub commands: Vec<StoredCommand>,
    pub selected_command_index: usize,
    pub command_list_state: ListState,
    // Feedback queue view
    pub selected_queue_index: usize,
    // Branch picker (rebase, local-merge, etc.)
    pub rebase_branches: Vec<String>,
    pub selected_rebase_branch_index: usize,
    pub pending_branch_command: Option<StoredCommand>,
    // Delete mode chooser
    pub delete_mode_index: usize,
    // Restart task wizard
    pub restart_wizard: Option<RestartWizard>,
    // Restart modal state (binary restart)
    pub restart_pending: bool,
    pub restart_confirm_index: usize,
    pub should_restart: bool,
    // PR polling
    pub last_pr_poll: Instant,
    pr_poll_tx: tokio_mpsc::UnboundedSender<Vec<PrPollResult>>,
    pr_poll_rx: tokio_mpsc::UnboundedReceiver<Vec<PrPollResult>>,
    pr_poll_active: bool,
    // Tokio runtime for background async work
    rt: tokio::runtime::Runtime,
    // Set linked PR modal
    pub pr_number_editor: TextArea<'static>,
    pub pr_owned_toggle: bool,
    // Directory picker for repos_dir
    pub dir_picker: Option<DirectoryPicker>,
    // Sleep inhibition (macOS: caffeinate -dis for idle, display, and system sleep assertions)
    #[cfg(target_os = "macos")]
    caffeinate_process: Option<std::process::Child>,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let tasks = Task::list_all(&config)?;
        let commands = StoredCommand::list_all(&config.commands_dir).unwrap_or_default();
        let notes_editor = VimTextArea::new();
        let feedback_editor = VimTextArea::new();
        let task_file_editor = VimTextArea::new();
        let (pr_poll_tx, pr_poll_rx) = tokio_mpsc::unbounded_channel();
        let rt = tokio::runtime::Runtime::new()?;

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
            review_wizard: None,
            output_log: Vec::new(),
            output_scroll: 0,
            last_output_time: None,
            task_file_content: String::new(),
            task_file_editor,
            answering_questions: false,
            commands,
            selected_command_index: 0,
            command_list_state: ListState::default(),
            selected_queue_index: 0,
            rebase_branches: Vec::new(),
            selected_rebase_branch_index: 0,
            pending_branch_command: None,
            delete_mode_index: 0,
            restart_wizard: None,
            restart_pending: false,
            restart_confirm_index: 0,
            should_restart: false,
            last_pr_poll: Instant::now(),
            pr_poll_tx,
            pr_poll_rx,
            pr_poll_active: false,
            rt,
            pr_number_editor: Self::create_plain_editor(),
            pr_owned_toggle: true,
            dir_picker: None,
            #[cfg(target_os = "macos")]
            caffeinate_process: std::process::Command::new("caffeinate")
                .arg("-dis")
                .spawn()
                .ok(),
        })
    }

    #[cfg(target_os = "macos")]
    fn stop_caffeinate(&mut self) {
        if let Some(ref mut child) = self.caffeinate_process {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.caffeinate_process = None;
    }

    #[cfg(not(target_os = "macos"))]
    fn stop_caffeinate(&mut self) {}

    fn create_plain_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(ratatui::style::Style::default());
        editor
    }

    /// Auto-wrap text in a VimTextArea when lines exceed max_width.
    fn auto_wrap_vim_editor(editor: &mut VimTextArea<'static>, max_width: usize) {
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
        let wrap_at = current_line[..max_width].rfind(' ').unwrap_or(max_width);

        if wrap_at == 0 {
            return;
        }

        // We need to split the line: move cursor to wrap point, insert newline
        let mut new_lines: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i == row {
                // Split this line
                let (before, after) = line.split_at(wrap_at);
                new_lines.push(before.to_string());
                new_lines.push(after.trim_start().to_string());
            } else {
                new_lines.push(line.clone());
            }
        }

        // Calculate new cursor position
        let new_col = if col > wrap_at {
            col - wrap_at - 1 // Account for removed space
        } else {
            col
        };
        let new_row = if col > wrap_at { row + 1 } else { row };

        // Save vim mode before recreating
        let current_mode = editor.mode();

        // Recreate the editor with new content
        editor.set_content(&new_lines.join("\n"));
        editor.vim.mode = current_mode;

        // Restore cursor position
        editor.move_cursor(CursorMove::Jump(new_row as u16, new_col as u16));
    }

    /// Hard-wrap all lines in a string to a given max width at word boundaries.
    /// Unlike `auto_wrap_vim_editor` which operates on a single line during editing,
    /// this wraps all lines in bulk — useful for pre-wrapping content before loading.
    fn wrap_content(text: &str, max_width: usize) -> String {
        if max_width < 10 {
            return text.to_string();
        }

        let mut result = Vec::new();
        for line in text.lines() {
            if line.len() <= max_width {
                result.push(line.to_string());
            } else {
                let mut remaining = line;
                while remaining.len() > max_width {
                    let wrap_at = remaining[..max_width]
                        .rfind(' ')
                        .unwrap_or(max_width);
                    if wrap_at == 0 {
                        // No space found, force break at max_width
                        result.push(remaining[..max_width].to_string());
                        remaining = &remaining[max_width..];
                    } else {
                        result.push(remaining[..wrap_at].to_string());
                        remaining = &remaining[wrap_at..].trim_start();
                    }
                }
                if !remaining.is_empty() {
                    result.push(remaining.to_string());
                }
            }
        }
        result.join("\n")
    }

    pub fn refresh_tasks(&mut self) -> Result<()> {
        let prev_task_id = self.selected_task().map(|t| t.meta.task_id());
        self.tasks = Task::list_all(&self.config)?;
        if let Some(ref id) = prev_task_id {
            if let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == *id) {
                self.selected_index = idx;
                return Ok(());
            }
        }
        if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_index = self.tasks.len() - 1;
        }
        Ok(())
    }

    /// Refresh the task list and restore selection to the task with the given ID.
    /// If the task is no longer present, selection falls back to a valid index.
    fn refresh_tasks_and_select(&mut self, task_id: &str) -> Result<()> {
        self.refresh_tasks()?;
        if let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == task_id) {
            self.selected_index = idx;
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
        self.last_output_time = Some(Instant::now());
        // Keep only the last 100 lines
        if self.output_log.len() > 100 {
            self.output_log.remove(0);
        }
        // Auto-scroll to bottom
        self.output_scroll = self.output_log.len().saturating_sub(5) as u16;
    }

    pub fn clear_old_status(&mut self) {
        if let Some((_, instant)) = &self.status_message {
            if instant.elapsed() > Duration::from_secs(3) {
                self.status_message = None;
            }
        }
    }

    /// Process any stranded feedback queues for stopped tasks.
    /// This is a safety net: if feedback was queued while a task was running and
    /// the agent process exited without processing it, the TUI picks it up.
    pub fn process_stranded_feedback(&mut self) {
        // Collect (index, task_id) for stopped tasks with queued feedback
        let stranded: Vec<(usize, String)> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.meta.status == TaskStatus::Stopped && t.has_queued_feedback())
            .map(|(i, t)| (i, t.meta.task_id()))
            .collect();

        for (_, task_id) in stranded {
            // Pop the first queued feedback item and write it as immediate feedback
            match self
                .tasks
                .iter()
                .find(|t| t.meta.task_id() == task_id)
            {
                Some(task) => match use_cases::pop_and_apply_feedback(task) {
                    Ok(Some(_)) => {}
                    Ok(None) => continue,
                    Err(e) => {
                        self.log_output(format!("Error processing feedback for {}: {}", task_id, e));
                        continue;
                    }
                },
                None => continue,
            }

            self.log_output(format!(
                "Processing stranded feedback for {}...",
                task_id
            ));

            let output = Command::new("agman")
                .args(["continue", &task_id])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    if !stdout.is_empty() {
                        for line in stdout.lines() {
                            self.log_output(line.to_string());
                        }
                    }
                    self.log_output(format!("Stranded feedback processed for {}", task_id));
                    self.set_status(format!("Processing stranded feedback for {}", task_id));
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    self.log_output(format!(
                        "Failed to process stranded feedback for {}: {}",
                        task_id, stderr
                    ));
                }
                Err(e) => {
                    self.log_output(format!("Error processing stranded feedback: {}", e));
                }
            }

            // Only process one stranded item per refresh cycle to avoid blocking the TUI
            break;
        }
    }

    pub fn clear_old_output(&mut self) {
        if let Some(instant) = &self.last_output_time {
            if instant.elapsed() > Duration::from_secs(7) {
                self.output_log.clear();
                self.output_scroll = 0;
                self.last_output_time = None;
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
        let (preview_content, notes_content, task_file_content) =
            if let Some(task) = self.selected_task() {
                let preview = task
                    .read_agent_log_structured_tail(500)
                    .unwrap_or_else(|_| "No agent log available".to_string());
                let notes = task.read_notes().unwrap_or_default();
                let task_file = task
                    .read_task()
                    .unwrap_or_else(|_| "No TASK.md available".to_string());
                (preview, notes, task_file)
            } else {
                return;
            };

        self.preview_content = preview_content;
        // Request scroll to bottom — will be clamped to the actual max on next render
        self.preview_scroll = u16::MAX;
        self.notes_content = notes_content.clone();
        self.notes_scroll = 0;

        // Setup notes editor with vim mode
        self.notes_editor = VimTextArea::from_lines(notes_content.lines());
        // Move cursor to end of text
        self.notes_editor.move_cursor(CursorMove::Bottom);
        self.notes_editor.move_cursor(CursorMove::End);
        self.notes_editing = false;

        // Setup task file content for modal (editor gets set up when modal opens)
        self.task_file_content = task_file_content;
    }

    fn save_notes(&mut self) -> Result<()> {
        if let Some(task) = self.selected_task() {
            let notes = self.notes_editor.lines_joined();
            use_cases::save_notes(task, &notes)?;
            self.notes_content = notes;
            self.set_status("Notes saved".to_string());
        }
        Ok(())
    }

    fn save_task_file(&mut self) -> Result<()> {
        if let Some(task) = self.selected_task() {
            let content = self.task_file_editor.lines_joined();
            use_cases::save_task_file(task, &content)?;
            self.task_file_content = content;
            self.set_status("TASK.md saved".to_string());
        }
        Ok(())
    }

    fn stop_task(&mut self) -> Result<()> {
        let task_info = self.selected_task().map(|t| {
            (
                t.meta.task_id(),
                t.meta.tmux_session.clone(),
                t.meta.status,
            )
        });

        if let Some((task_id, tmux_session, status)) = task_info {
            if status == TaskStatus::Stopped {
                self.set_status(format!("Task already stopped: {}", task_id));
                return Ok(());
            }

            tracing::info!(task_id = %task_id, "TUI: stop task requested");
            self.log_output(format!("Stopping task {}...", task_id));

            // Send Ctrl+C to the agman window to interrupt any running process (side effect)
            if Tmux::session_exists(&tmux_session) {
                match Tmux::send_ctrl_c_to_window(&tmux_session, "agman") {
                    Ok(_) => {
                        self.log_output("  Sent interrupt signal to agman window".to_string());
                    }
                    Err(e) => {
                        self.log_output(format!(
                            "  Warning: Could not interrupt agman window: {}",
                            e
                        ));
                    }
                }
            }

            // Delegate business logic to use_cases
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                if let Err(e) = use_cases::stop_task(task) {
                    self.log_output(format!("  Error stopping task: {}", e));
                    self.set_status(format!("Error: {}", e));
                    return Ok(());
                }
            }

            self.set_status(format!("Stopped: {}", task_id));
        }
        Ok(())
    }

    fn toggle_hold(&mut self) -> Result<()> {
        let task_info = self
            .selected_task()
            .map(|t| (t.meta.task_id(), t.meta.status));

        if let Some((task_id, status)) = task_info {
            match status {
                TaskStatus::Stopped => {
                    tracing::info!(task_id = %task_id, "TUI: put on hold requested");
                    if let Some(task) = self.tasks.get_mut(self.selected_index) {
                        use_cases::put_on_hold(task)?;
                    }
                    self.set_status(format!("On hold: {}", task_id));
                    self.refresh_tasks_and_select(&task_id)?;
                }
                TaskStatus::OnHold => {
                    tracing::info!(task_id = %task_id, "TUI: resume from hold requested");
                    if let Some(task) = self.tasks.get_mut(self.selected_index) {
                        use_cases::resume_from_hold(task)?;
                    }
                    self.set_status(format!("Resumed: {}", task_id));
                    self.refresh_tasks_and_select(&task_id)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn open_set_linked_pr(&mut self) {
        let mut editor = Self::create_plain_editor();
        let mut owned = true;
        if let Some(task) = self.selected_task() {
            if let Some(pr) = &task.meta.linked_pr {
                editor = TextArea::new(vec![pr.number.to_string()]);
                editor.set_cursor_line_style(ratatui::style::Style::default());
                owned = pr.owned;
            }
        }
        self.pr_number_editor = editor;
        self.pr_owned_toggle = owned;
        self.view = View::SetLinkedPr;
    }

    fn resume_after_answering(&mut self) -> Result<()> {
        let task_info = self.selected_task().map(|t| {
            (
                t.meta.task_id(),
                t.meta.tmux_session.clone(),
                t.meta.status,
            )
        });

        if let Some((task_id, tmux_session, status)) = task_info {
            if status != TaskStatus::InputNeeded {
                return Ok(());
            }

            tracing::info!(task_id = %task_id, "TUI: resume after answering");
            // Delegate business logic to use_cases
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                use_cases::resume_after_answering(task)?;
                let _ = use_cases::set_review_addressed(task, false);
            }

            // Side effects: ensure tmux session and dispatch flow
            if !Tmux::session_exists(&tmux_session) {
                if let Some(task) = self.selected_task() {
                    let _ = Tmux::create_session_with_windows(
                        &tmux_session,
                        &task.meta.worktree_path,
                    );
                    let _ = Tmux::add_review_window(&tmux_session, &task.meta.worktree_path);
                }
            }

            let flow_cmd = format!("agman flow-run {}", task_id);
            let _ = Tmux::send_keys_to_window(&tmux_session, "agman", &flow_cmd);

            self.log_output(format!("Resumed flow for {} — processing your answers", task_id));
            self.set_status(format!("Resumed: {}", task_id));
            self.refresh_tasks_and_select(&task_id)?;
        }

        Ok(())
    }

    fn delete_task(&mut self, mode: DeleteMode) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let task = self.tasks.remove(self.selected_index);
        let task_id = task.meta.task_id();
        let tmux_session = task.meta.tmux_session.clone();

        let uc_mode = match mode {
            DeleteMode::Everything => use_cases::DeleteMode::Everything,
            DeleteMode::TaskOnly => use_cases::DeleteMode::TaskOnly,
        };

        tracing::info!(task_id = %task_id, mode = ?mode, "TUI: delete task requested");
        self.log_output(format!("Deleting task {} ({})...", task_id, match mode {
            DeleteMode::Everything => "everything",
            DeleteMode::TaskOnly => "task only",
        }));

        // Kill tmux session (side effect)
        let _ = Tmux::kill_session(&tmux_session);
        self.log_output("  Killed tmux session".to_string());

        // Delegate business logic to use_cases
        use_cases::delete_task(&self.config, task, uc_mode)?;
        self.log_output("  Deleted task files".to_string());

        if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_index = self.tasks.len() - 1;
        }

        self.set_status(format!("Deleted: {}", task_id));
        self.view = View::TaskList;
        Ok(())
    }

    fn start_feedback(&mut self) {
        // Clear the feedback editor and start in insert mode
        self.feedback_editor = VimTextArea::new();
        self.feedback_editor.set_insert_mode();
        self.view = View::Feedback;
    }

    fn submit_feedback(&mut self) -> Result<()> {
        let feedback = self.feedback_editor.lines_joined();
        let task_id_for_log = self.selected_task().map(|t| t.meta.task_id());
        tracing::info!(task_id = ?task_id_for_log, "submitting feedback");
        if feedback.trim().is_empty() {
            self.set_status("Feedback cannot be empty".to_string());
            self.view = View::Preview;
            return Ok(());
        }

        // Reload task status from disk before deciding whether to queue or execute,
        // since the Feedback view doesn't refresh and the task may have stopped while
        // the user was typing.
        if let Some(task) = self.tasks.get_mut(self.selected_index) {
            let _ = task.reload_meta();
        }

        // Check if task is running - if so, queue the feedback instead
        let (task_id, is_running) = if let Some(task) = self.selected_task() {
            (task.meta.task_id(), task.meta.status == TaskStatus::Running)
        } else {
            self.set_status("No task selected".to_string());
            self.view = View::TaskList;
            return Ok(());
        };

        if is_running {
            // Delegate to use_cases: queue the feedback
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                let queue_count = use_cases::queue_feedback(task, &feedback)?;
                self.log_output(format!("Queued feedback for {} ({} in queue)", task_id, queue_count));
                self.set_status(format!("Feedback queued ({} in queue)", queue_count));
            }
        } else {
            // Clear review_addressed on user interaction
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                let _ = use_cases::set_review_addressed(task, false);
            }

            // Delegate to use_cases: write immediate feedback
            if let Some(task) = self.selected_task() {
                use_cases::write_immediate_feedback(task, &feedback)?;
            }

            self.log_output(format!("Starting continue flow for {}...", task_id));

            // Run agman continue (reads feedback from FEEDBACK.md)
            // Capture output to avoid corrupting TUI
            let output = Command::new("agman")
                .args(["continue", &task_id])
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
                    self.refresh_tasks_and_select(&task_id)?;
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
        }

        self.feedback_editor = VimTextArea::new(); // Clear editor
        self.view = View::Preview;
        self.load_preview();
        Ok(())
    }

    // === Wizard Methods ===

    fn start_wizard(&mut self) -> Result<()> {
        let repos = self.scan_repos()?;

        if repos.is_empty() {
            // Launch directory picker so the user can choose a repos directory
            let start = if self.config.repos_dir.exists() {
                self.config.repos_dir.clone()
            } else {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
            };
            self.dir_picker = Some(DirectoryPicker::new(start, DirPickerOrigin::NewTask));
            self.view = View::DirectoryPicker;
            self.set_status(format!("No repos found in {}. Pick a repos directory (s to select, h/l to navigate).", self.config.repos_dir.display()));
            return Ok(());
        }

        // Load repo stats and filter favorites to only repos that still exist
        let stats = RepoStats::load(&self.config.repo_stats_path());
        let repos_set: std::collections::HashSet<&str> =
            repos.iter().map(|s| s.as_str()).collect();
        let favorite_repos: Vec<(String, u64)> = stats
            .favorites()
            .into_iter()
            .filter(|(name, _)| repos_set.contains(name.as_str()))
            .collect();

        let mut new_branch_editor = Self::create_plain_editor();
        new_branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        // Description editor uses vim mode, start in insert mode
        let mut description_editor = VimTextArea::new();
        description_editor.set_insert_mode();

        self.wizard = Some(NewTaskWizard {
            step: WizardStep::SelectRepo,
            repos,
            favorite_repos,
            selected_repo_index: 0,
            branch_source: BranchSource::NewBranch,
            existing_worktrees: Vec::new(),
            selected_worktree_index: 0,
            existing_branches: Vec::new(),
            selected_branch_index: 0,
            new_branch_editor,
            description_editor,
            error_message: None,
            review_after: false,
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

    pub fn scan_commands(&mut self) -> Result<()> {
        self.commands = StoredCommand::list_all(&self.config.commands_dir)?;
        if self.selected_command_index >= self.commands.len() && !self.commands.is_empty() {
            self.selected_command_index = self.commands.len() - 1;
        }
        Ok(())
    }

    fn scan_rebase_branches(&self, repo_name: &str, local_only: bool) -> Result<Vec<String>> {
        let repo_path = self.config.repo_path(repo_name);

        // Get local branches
        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "--list", "--format=%(refname:short)"])
            .output()?;

        let mut branches: Vec<String> = if output.status.success() {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        };

        if !local_only {
            // Also get remote-tracking branches
            let remote_output = Command::new("git")
                .current_dir(&repo_path)
                .args(["branch", "-r", "--format=%(refname:short)"])
                .output()?;

            if remote_output.status.success() {
                let remote_stdout = String::from_utf8_lossy(&remote_output.stdout);
                for line in remote_stdout.lines() {
                    let branch = line.trim();
                    // Skip HEAD pointers like origin/HEAD
                    if branch.contains("HEAD") {
                        continue;
                    }
                    // Only add if not already represented by a local branch
                    // e.g., if we have "main" locally, don't add "origin/main"
                    let short_name = branch.split('/').skip(1).collect::<Vec<_>>().join("/");
                    if !branches.contains(&short_name) {
                        branches.push(branch.to_string());
                    }
                }
            }
        }

        branches.sort();
        branches.dedup();

        tracing::debug!(repo = %repo_name, count = branches.len(), local_only, "scanned branches for picker");

        Ok(branches)
    }

    fn open_branch_picker(&mut self) {
        let repo_name = match self.selected_task() {
            Some(t) => t.meta.repo_name.clone(),
            None => {
                self.set_status("No task selected".to_string());
                return;
            }
        };

        let local_only = self
            .pending_branch_command
            .as_ref()
            .map(|c| c.id == "local-merge")
            .unwrap_or(false);

        match self.scan_rebase_branches(&repo_name, local_only) {
            Ok(branches) => {
                if branches.is_empty() {
                    self.set_status("No branches found".to_string());
                    return;
                }

                // Preselect main/master for local-merge command
                let preselect_index = if self
                    .pending_branch_command
                    .as_ref()
                    .map(|c| c.id == "local-merge")
                    .unwrap_or(false)
                {
                    branches
                        .iter()
                        .position(|b| b == "main" || b == "master")
                        .unwrap_or(0)
                } else {
                    0
                };

                self.rebase_branches = branches;
                self.selected_rebase_branch_index = preselect_index;
                self.view = View::RebaseBranchPicker;
            }
            Err(e) => {
                self.set_status(format!("Error scanning branches: {}", e));
            }
        }
    }

    fn run_branch_command(&mut self, branch: &str) -> Result<()> {
        let command = match self.pending_branch_command.take() {
            Some(c) => c,
            None => {
                self.set_status("No pending branch command".to_string());
                self.view = View::Preview;
                return Ok(());
            }
        };

        let task_id = match self.selected_task() {
            Some(t) => t.meta.task_id(),
            None => {
                self.set_status("No task selected".to_string());
                self.view = View::Preview;
                return Ok(());
            }
        };

        self.log_output(format!(
            "Running '{}' with branch '{}' for task {}...",
            command.name, branch, task_id
        ));

        let output = Command::new("agman")
            .args(["run-command", &task_id, &command.id, "--branch", branch])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if !stdout.is_empty() {
                    for line in stdout.lines() {
                        self.log_output(line.to_string());
                    }
                }
                self.refresh_tasks_and_select(&task_id)?;
                self.set_status(format!("Started: {} onto {}", command.name, branch));
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

        self.view = View::Preview;
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
        self.command_list_state.select(Some(0));
        self.view = View::CommandList;
    }

    fn run_selected_command(&mut self) -> Result<()> {
        let task_id_for_log = self.selected_task().map(|t| t.meta.task_id());
        let command = match self.commands.get(self.selected_command_index) {
            Some(c) => {
                tracing::info!(task_id = ?task_id_for_log, command = %c.name, "running stored command");
                c.clone()
            }
            None => {
                self.set_status("No command selected".to_string());
                self.view = View::TaskList;
                return Ok(());
            }
        };

        // If the command requires a branch argument, open the branch picker
        if command.requires_arg.as_deref() == Some("branch") {
            self.pending_branch_command = Some(command);
            self.open_branch_picker();
            return Ok(());
        }

        let task_id = match self.selected_task() {
            Some(t) => t.meta.task_id(),
            None => {
                self.set_status("No task selected".to_string());
                self.view = View::TaskList;
                return Ok(());
            }
        };

        // Guard: refuse create-pr if a PR is already linked
        if command.id == "create-pr" {
            if let Some(task) = self.selected_task() {
                if let Some(ref pr) = task.meta.linked_pr {
                    self.set_status(format!("PR #{} already linked — use monitor-pr instead.", pr.number));
                    self.view = View::Preview;
                    return Ok(());
                }
            }
        }

        self.log_output(format!(
            "Running command '{}' on task {}...",
            command.name, task_id
        ));

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
                // Update review_addressed flag based on which command was run
                if let Some(task) = self.tasks.iter_mut().find(|t| t.meta.task_id() == task_id) {
                    if command.id == "address-review" {
                        let _ = use_cases::set_review_addressed(task, true);
                    } else {
                        let _ = use_cases::set_review_addressed(task, false);
                    }
                }
                self.refresh_tasks_and_select(&task_id)?;
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

        self.view = View::Preview;
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

    fn scan_existing_worktrees(&self, repo_name: &str) -> Result<Vec<(String, PathBuf)>> {
        let repo_path = self.config.repo_path(repo_name);
        let worktrees = Git::list_worktrees(&repo_path)?;

        // Build set of branches that already have tasks for this repo
        let existing_tasks: std::collections::HashSet<String> = self
            .tasks
            .iter()
            .filter(|t| t.meta.repo_name == repo_name)
            .map(|t| t.meta.branch_name.clone())
            .collect();

        let main_repo_path = repo_path.canonicalize().unwrap_or(repo_path);

        let orphans: Vec<(String, PathBuf)> = worktrees
            .into_iter()
            .filter(|(branch, path)| {
                // Filter out the main repo worktree
                let canonical = path.canonicalize().unwrap_or(path.clone());
                if canonical == main_repo_path {
                    return false;
                }
                // Filter out worktrees that already have a task
                !existing_tasks.contains(branch)
            })
            .collect();

        Ok(orphans)
    }

    fn wizard_next_step(&mut self) -> Result<()> {
        let wizard = match &mut self.wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        wizard.error_message = None;

        match wizard.step {
            WizardStep::SelectRepo => {
                // Scan both branches and existing worktrees for selected repo
                let repo_name = wizard.selected_repo_name().to_string();
                let branches = self.scan_branches(&repo_name)?;
                let existing_wts = self.scan_existing_worktrees(&repo_name)?;

                let wizard = self.wizard.as_mut().unwrap();
                wizard.existing_branches = branches;
                wizard.selected_branch_index = 0;
                wizard.existing_worktrees = existing_wts;
                wizard.selected_worktree_index = 0;
                wizard.branch_source = BranchSource::NewBranch;
                wizard.step = WizardStep::SelectBranch;
            }
            WizardStep::SelectBranch => {
                // Validate based on branch source
                let branch_name = match wizard.branch_source {
                    BranchSource::NewBranch => {
                        let name = wizard.new_branch_editor.lines().join("");
                        let name = name.trim().to_string();
                        if name.is_empty() {
                            wizard.error_message = Some("Branch name cannot be empty".to_string());
                            return Ok(());
                        }
                        if name.contains(' ')
                            || name.contains("..")
                            || name.starts_with('/')
                            || name.ends_with('/')
                        {
                            wizard.error_message = Some("Invalid branch name format".to_string());
                            return Ok(());
                        }
                        name
                    }
                    BranchSource::ExistingBranch => {
                        if wizard.existing_branches.is_empty() {
                            wizard.error_message =
                                Some("No existing branches available".to_string());
                            return Ok(());
                        }
                        wizard.existing_branches[wizard.selected_branch_index].clone()
                    }
                    BranchSource::ExistingWorktree => {
                        if wizard.existing_worktrees.is_empty() {
                            wizard.error_message =
                                Some("No existing worktrees without tasks".to_string());
                            return Ok(());
                        }
                        let (ref branch, _) =
                            wizard.existing_worktrees[wizard.selected_worktree_index];
                        branch.clone()
                    }
                };

                // Check if task already exists
                let repo_name = wizard.selected_repo_name().to_string();
                let task_dir = self.config.task_dir(&repo_name, &branch_name);
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
                let description = wizard.description_editor.lines_joined();
                let description = description.trim();
                if description.is_empty() {
                    wizard.error_message = Some("Description cannot be empty".to_string());
                    return Ok(());
                }
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
        }
    }

    fn create_task_from_wizard(&mut self) -> Result<()> {
        let wizard = match &self.wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        let repo_name = wizard.selected_repo_name().to_string();

        let (branch_name, worktree_source) = match wizard.branch_source {
            BranchSource::ExistingWorktree => {
                let (branch, path) =
                    wizard.existing_worktrees[wizard.selected_worktree_index].clone();
                (branch, use_cases::WorktreeSource::ExistingWorktree(path))
            }
            BranchSource::NewBranch => {
                let name = wizard.new_branch_editor.lines().join("").trim().to_string();
                (name, use_cases::WorktreeSource::NewBranch)
            }
            BranchSource::ExistingBranch => {
                let name = wizard.existing_branches[wizard.selected_branch_index].clone();
                (name, use_cases::WorktreeSource::ExistingBranch)
            }
        };

        let description = wizard.description_editor.lines_joined().trim().to_string();
        let review_after = wizard.review_after;

        tracing::info!(repo = %repo_name, branch = %branch_name, "creating task via wizard");
        self.log_output(format!("Creating task {}--{}...", repo_name, branch_name));

        // Delegate business logic to use_cases
        let task = match use_cases::create_task(
            &self.config,
            &repo_name,
            &branch_name,
            &description,
            "new",
            worktree_source,
            review_after,
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

        // Side effects: create tmux session and start flow
        let worktree_path = task.meta.worktree_path.clone();
        self.log_output("  Creating tmux session...".to_string());
        if let Err(e) = Tmux::create_session_with_windows(&task.meta.tmux_session, &worktree_path) {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        let _ = Tmux::add_review_window(&task.meta.tmux_session, &worktree_path);

        let task_id = task.meta.task_id();
        let flow_cmd = format!("agman flow-run {}", task_id);
        let _ = Tmux::send_keys_to_window(&task.meta.tmux_session, "agman", &flow_cmd);

        // Success - close wizard and refresh
        self.wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id)?;
        self.set_status(format!("Created task: {}", task_id));

        Ok(())
    }

    // === Review Wizard Methods ===

    fn start_review_wizard(&mut self) -> Result<()> {
        let repos = self.scan_repos()?;

        if repos.is_empty() {
            // Launch directory picker so the user can choose a repos directory
            let start = if self.config.repos_dir.exists() {
                self.config.repos_dir.clone()
            } else {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
            };
            self.dir_picker = Some(DirectoryPicker::new(start, DirPickerOrigin::Review));
            self.view = View::DirectoryPicker;
            self.set_status(format!("No repos found in {}. Pick a repos directory (s to select, h/l to navigate).", self.config.repos_dir.display()));
            return Ok(());
        }

        // Load repo stats and filter favorites to only repos that still exist
        let stats = RepoStats::load(&self.config.repo_stats_path());
        let repos_set: std::collections::HashSet<&str> =
            repos.iter().map(|s| s.as_str()).collect();
        let favorite_repos: Vec<(String, u64)> = stats
            .favorites()
            .into_iter()
            .filter(|(name, _)| repos_set.contains(name.as_str()))
            .collect();

        let mut branch_editor = Self::create_plain_editor();
        branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        self.review_wizard = Some(ReviewWizard {
            step: ReviewWizardStep::SelectRepo,
            repos,
            favorite_repos,
            selected_repo_index: 0,
            branch_source: BranchSource::NewBranch,
            branch_editor,
            existing_branches: Vec::new(),
            selected_branch_index: 0,
            existing_worktrees: Vec::new(),
            selected_worktree_index: 0,
            error_message: None,
        });

        self.view = View::ReviewWizard;
        Ok(())
    }

    fn review_wizard_next_step(&mut self) -> Result<()> {
        let wizard = match &mut self.review_wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        wizard.error_message = None;

        match wizard.step {
            ReviewWizardStep::SelectRepo => {
                // Scan branches and worktrees for the selected repo
                let repo_name = wizard.selected_repo_name().to_string();
                let branches = self.scan_branches(&repo_name)?;
                let existing_wts = self.scan_existing_worktrees(&repo_name)?;

                let wizard = self.review_wizard.as_mut().unwrap();
                wizard.existing_branches = branches;
                wizard.selected_branch_index = 0;
                wizard.existing_worktrees = existing_wts;
                wizard.selected_worktree_index = 0;
                wizard.branch_source = BranchSource::NewBranch;
                wizard.step = ReviewWizardStep::EnterBranch;
            }
            ReviewWizardStep::EnterBranch => {
                let branch_name = match wizard.branch_source {
                    BranchSource::NewBranch => {
                        let name = wizard.branch_editor.lines().join("");
                        let name = name.trim().to_string();
                        if name.is_empty() {
                            wizard.error_message =
                                Some("Branch name cannot be empty".to_string());
                            return Ok(());
                        }
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
                    BranchSource::ExistingBranch => {
                        if wizard.existing_branches.is_empty() {
                            wizard.error_message =
                                Some("No existing branches available".to_string());
                            return Ok(());
                        }
                        wizard.existing_branches[wizard.selected_branch_index].clone()
                    }
                    BranchSource::ExistingWorktree => {
                        if wizard.existing_worktrees.is_empty() {
                            wizard.error_message =
                                Some("No existing worktrees available".to_string());
                            return Ok(());
                        }
                        wizard.existing_worktrees[wizard.selected_worktree_index]
                            .0
                            .clone()
                    }
                };

                // Check if task already exists
                let repo_name = wizard.selected_repo_name().to_string();
                let task_dir = self.config.task_dir(&repo_name, &branch_name);
                if task_dir.exists() {
                    wizard.error_message = Some(format!(
                        "Task '{}--{}' already exists",
                        repo_name, branch_name
                    ));
                    return Ok(());
                }

                return self.create_review_task();
            }
        }

        Ok(())
    }

    fn review_wizard_prev_step(&mut self) {
        let wizard = match &mut self.review_wizard {
            Some(w) => w,
            None => return,
        };

        wizard.error_message = None;

        match wizard.step {
            ReviewWizardStep::SelectRepo => {
                self.review_wizard = None;
                self.view = View::TaskList;
            }
            ReviewWizardStep::EnterBranch => {
                wizard.step = ReviewWizardStep::SelectRepo;
            }
        }
    }

    fn create_review_task(&mut self) -> Result<()> {
        let wizard = match &self.review_wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        let repo_name = wizard.selected_repo_name().to_string();
        let (branch_name, worktree_source) = match wizard.branch_source {
            BranchSource::ExistingWorktree => {
                let (branch, path) =
                    wizard.existing_worktrees[wizard.selected_worktree_index].clone();
                (branch, use_cases::WorktreeSource::ExistingWorktree(path))
            }
            BranchSource::NewBranch => {
                let name = wizard.branch_editor.lines().join("").trim().to_string();
                (name, use_cases::WorktreeSource::ExistingBranch)
            }
            BranchSource::ExistingBranch => {
                let name = wizard.existing_branches[wizard.selected_branch_index].clone();
                (name, use_cases::WorktreeSource::ExistingBranch)
            }
        };

        self.log_output(format!(
            "Creating review task {}--{}...",
            repo_name, branch_name
        ));

        // Delegate business logic to use_cases
        let mut task = match use_cases::create_review_task(
            &self.config,
            &repo_name,
            &branch_name,
            worktree_source,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.review_wizard {
                    w.error_message = Some(format!("Failed to create review task: {}", e));
                }
                return Ok(());
            }
        };

        // Best-effort: look up the PR for this branch and link it
        if let Some(pr_number) = lookup_pr_for_branch(&task.meta.worktree_path, &branch_name) {
            let task_id = task.meta.task_id();
            let wt = task.meta.worktree_path.clone();
            match use_cases::set_linked_pr(&mut task, pr_number, &wt, false) {
                Ok(()) => {
                    tracing::info!(task_id = %task_id, pr_number, branch = %branch_name, "linked PR to review task");
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, branch = %branch_name, error = %e, "failed to set linked PR");
                }
            }
        } else {
            tracing::debug!(branch = %branch_name, "no PR found for branch, skipping PR link");
        }

        // Side effects: create tmux session and run review command
        let worktree_path = task.meta.worktree_path.clone();
        self.log_output("  Creating tmux session...".to_string());
        if let Err(e) =
            Tmux::create_session_with_windows(&task.meta.tmux_session, &worktree_path)
        {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.review_wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        Tmux::add_review_window(&task.meta.tmux_session, &worktree_path)?;

        let task_id = task.meta.task_id();
        let review_cmd = format!("agman run-command {} review-pr", task_id);
        let _ = Tmux::send_keys_to_window(&task.meta.tmux_session, "agman", &review_cmd);

        // Success - close wizard and refresh
        self.review_wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id)?;
        self.set_status(format!("Review started: {}", task_id));

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
            View::TaskEditor => self.handle_task_editor_event(event),
            View::FeedbackQueue => self.handle_feedback_queue_event(event),
            View::RebaseBranchPicker => self.handle_rebase_branch_picker_event(event),
            View::ReviewWizard => self.handle_review_wizard_event(event),
            View::RestartConfirm => self.handle_restart_confirm_event(event),
            View::RestartWizard => self.handle_restart_wizard_event(event),
            View::SetLinkedPr => self.handle_set_linked_pr_event(event),
            View::DirectoryPicker => self.handle_directory_picker_event(event),
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
                KeyCode::Enter => {
                    self.load_preview();
                    self.preview_pane = PreviewPane::Logs;
                    self.view = View::Preview;
                }
                KeyCode::Char('S') => {
                    self.stop_task()?;
                }
                KeyCode::Char('d') => {
                    if !self.tasks.is_empty() {
                        self.delete_mode_index = 0;
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
                KeyCode::Char('r') => {
                    // Start review wizard
                    self.start_review_wizard()?;
                }
                KeyCode::Char('x') => {
                    // Open command list (go to preview first, like f and t)
                    if !self.tasks.is_empty() {
                        self.load_preview();
                        self.open_command_list();
                    }
                }
                KeyCode::Char('t') => {
                    // Open task editor modal
                    if !self.tasks.is_empty() {
                        self.load_preview();
                        self.open_task_editor();
                    }
                }
                KeyCode::Char('a') => {
                    // Answer questions (only for InputNeeded tasks)
                    if let Some(task) = self.selected_task() {
                        if task.meta.status == TaskStatus::InputNeeded {
                            self.load_preview();
                            self.open_task_editor_for_answering();
                        }
                    }
                }
                KeyCode::Char('o') => {
                    // Open linked PR in browser
                    let pr_info = self.selected_task().and_then(|t| {
                        t.meta.linked_pr.as_ref().map(|pr| (pr.number, pr.url.clone()))
                    });
                    if let Some((number, url)) = pr_info {
                        open_url(&url);
                        self.set_status(format!("Opening PR #{}...", number));
                    } else {
                        self.set_status("No linked PR".to_string());
                    }
                }
                KeyCode::Char('W') => {
                    // Restart task wizard
                    self.start_restart_wizard()?;
                }
                KeyCode::Char('U') => {
                    // Manual restart
                    self.restart_confirm_index = 0;
                    self.view = View::RestartConfirm;
                }
                KeyCode::Char('H') => {
                    self.toggle_hold()?;
                }
                KeyCode::Char('c') => {
                    // Toggle review_addressed indicator on selected task (owned PRs only)
                    let is_owned = self.selected_task().and_then(|t| {
                        t.meta.linked_pr.as_ref().map(|pr| pr.owned)
                    }).unwrap_or(false);
                    if is_owned {
                        if let Some(task) = self.tasks.get_mut(self.selected_index) {
                            let new_val = !task.meta.review_addressed;
                            let _ = use_cases::set_review_addressed(task, new_val);
                            if new_val {
                                self.set_status("Marked review addressed".to_string());
                            } else {
                                self.set_status("Cleared review indicator".to_string());
                            }
                        }
                    } else {
                        self.set_status("Review tracking only for owned PRs".to_string());
                    }
                }
                KeyCode::Char('P') => {
                    self.open_set_linked_pr();
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

            // Tab to switch panes (Logs -> Notes -> Logs)
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
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                KeyCode::Char('h') => {
                    self.preview_pane = PreviewPane::Logs;
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
                            self.notes_editor.set_insert_mode();
                            self.set_status(
                                "Editing notes (vim mode, Ctrl+S or Esc twice to save)".to_string(),
                            );
                        }
                    }
                }
                KeyCode::Char('i') => {
                    // Enter edit mode for notes
                    if self.preview_pane == PreviewPane::Notes {
                        self.notes_editing = true;
                        self.notes_editor.set_insert_mode();
                        self.set_status(
                            "Editing notes (vim mode, Ctrl+S or Esc twice to save)".to_string(),
                        );
                    }
                }
                KeyCode::Char('t') => {
                    // Open TaskEditor modal
                    self.open_task_editor();
                }
                KeyCode::Char('a') => {
                    // Answer questions (only for InputNeeded tasks)
                    if let Some(task) = self.selected_task() {
                        if task.meta.status == TaskStatus::InputNeeded {
                            self.open_task_editor_for_answering();
                        }
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
                KeyCode::Char('Q') => {
                    // Open feedback queue view
                    self.open_feedback_queue();
                }
                KeyCode::Char('j') => match self.preview_pane {
                    PreviewPane::Logs => {
                        self.preview_scroll = self.preview_scroll.saturating_add(1);
                    }
                    PreviewPane::Notes => {
                        self.notes_scroll = self.notes_scroll.saturating_add(1);
                    }
                },
                KeyCode::Char('k') => match self.preview_pane {
                    PreviewPane::Logs => {
                        self.preview_scroll = self.preview_scroll.saturating_sub(1);
                    }
                    PreviewPane::Notes => {
                        self.notes_scroll = self.notes_scroll.saturating_sub(1);
                    }
                },
                KeyCode::Char('l') => {
                    self.preview_pane = PreviewPane::Notes;
                }
                KeyCode::Char('G') => {
                    // Jump to bottom (clamped to actual max on next render)
                    match self.preview_pane {
                        PreviewPane::Logs => {
                            self.preview_scroll = u16::MAX;
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
                KeyCode::Char('S') => {
                    self.stop_task()?;
                }
                KeyCode::Char('o') => {
                    // Open linked PR in browser
                    let pr_info = self.selected_task().and_then(|t| {
                        t.meta.linked_pr.as_ref().map(|pr| (pr.number, pr.url.clone()))
                    });
                    if let Some((number, url)) = pr_info {
                        open_url(&url);
                        self.set_status(format!("Opening PR #{}...", number));
                    } else {
                        self.set_status("No linked PR".to_string());
                    }
                }
                KeyCode::Char('W') => {
                    self.start_restart_wizard()?;
                }
                KeyCode::Char('H') => {
                    self.toggle_hold()?;
                }
                KeyCode::Char('P') => {
                    self.open_set_linked_pr();
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_notes_editing(&mut self, event: Event) -> Result<bool> {
        // Calculate wrap width dynamically: notes panel is 30% of screen width, minus borders
        let wrap_width = crossterm::terminal::size()
            .map(|(w, _)| ((w as f32 * 0.30) as usize).saturating_sub(4))
            .unwrap_or(30);

        if let Event::Key(key) = event {
            // Check for Ctrl+S to save in any mode
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.notes_editing = false;
                self.notes_editor.set_normal_mode();
                self.save_notes()?;
                return Ok(false);
            }

            // In Normal mode, Esc exits editing; in Insert mode, Esc goes to Normal
            let input = Input::from(event.clone());
            let was_insert = self.notes_editor.mode() == VimMode::Insert;

            self.notes_editor.input(input.clone());

            // If we were in insert mode and now in normal, or got Esc in normal, might exit
            let is_normal_now = self.notes_editor.mode() == VimMode::Normal;

            // If we pressed Esc and are now in normal mode after being in normal, exit editing
            if input.key == Key::Esc && !was_insert && is_normal_now {
                self.notes_editing = false;
                self.save_notes()?;
                return Ok(false);
            }

            // Auto-wrap only in insert mode
            if self.notes_editor.mode() == VimMode::Insert {
                Self::auto_wrap_vim_editor(&mut self.notes_editor, wrap_width);
            }
        }
        Ok(false)
    }

    fn open_task_editor(&mut self) {
        self.answering_questions = false;
        self.open_task_editor_inner();
    }

    fn open_task_editor_for_answering(&mut self) {
        self.answering_questions = true;
        self.open_task_editor_inner();
    }

    fn open_task_editor_inner(&mut self) {
        // Re-read the task file content from disk to ensure fresh content
        if let Some(task) = self.selected_task() {
            let content = task
                .read_task()
                .unwrap_or_else(|_| "No TASK.md available".to_string());
            self.task_file_content = content.clone();

            // Pre-wrap content to fit the modal width (same formula as handle_task_editor_event)
            let wrap_width = crossterm::terminal::size()
                .map(|(w, _)| ((w as f32 * 0.80) as usize).saturating_sub(6))
                .unwrap_or(70);
            let wrapped = Self::wrap_content(&content, wrap_width);

            self.task_file_editor = VimTextArea::from_lines(wrapped.lines());
        }
        self.view = View::TaskEditor;
    }

    fn handle_task_editor_event(&mut self, event: Event) -> Result<bool> {
        // Calculate wrap width dynamically: modal is ~80% of screen width, minus borders
        let wrap_width = crossterm::terminal::size()
            .map(|(w, _)| ((w as f32 * 0.80) as usize).saturating_sub(6))
            .unwrap_or(70);

        if let Event::Key(key) = event {
            // Check for Ctrl+S to save and close in any mode
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.save_task_file()?;
                self.task_file_editor.set_normal_mode();

                // If we were answering questions, resume the flow
                if self.answering_questions {
                    self.answering_questions = false;
                    self.resume_after_answering()?;
                }

                self.view = View::Preview;
                return Ok(false);
            }

            let input = Input::from(event.clone());
            let was_insert = self.task_file_editor.mode() == VimMode::Insert;

            self.task_file_editor.input(input.clone());

            let is_normal_now = self.task_file_editor.mode() == VimMode::Normal;

            // If we pressed Esc while already in normal mode, cancel editing
            if input.key == Key::Esc && !was_insert && is_normal_now {
                self.view = View::Preview;
                self.set_status("Task editor cancelled".to_string());
                return Ok(false);
            }

            // Auto-wrap only in insert mode
            if self.task_file_editor.mode() == VimMode::Insert {
                Self::auto_wrap_vim_editor(&mut self.task_file_editor, wrap_width);
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
            // Check for Ctrl+S to submit in any mode
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.feedback_editor.set_normal_mode();
                self.submit_feedback()?;
                return Ok(false);
            }

            // Toggle review_after on the selected task with Ctrl+R
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
                if let Some(task) = self.tasks.get_mut(self.selected_index) {
                    task.meta.review_after = !task.meta.review_after;
                    let _ = task.save_meta();
                    let state = if task.meta.review_after { "ON" } else { "OFF" };
                    tracing::info!(task_id = %task.meta.task_id(), review_after = task.meta.review_after, "toggled review_after to {}", state);
                    self.set_status(format!("Review after flow: {}", state));
                }
                return Ok(false);
            }

            let input = Input::from(event.clone());
            let was_insert = self.feedback_editor.mode() == VimMode::Insert;

            self.feedback_editor.input(input.clone());

            let is_normal_now = self.feedback_editor.mode() == VimMode::Normal;

            // If we pressed Esc while already in normal mode, cancel feedback
            if input.key == Key::Esc && !was_insert && is_normal_now {
                self.view = View::Preview;
                self.set_status("Feedback cancelled".to_string());
                return Ok(false);
            }

            // Auto-wrap only in insert mode
            if self.feedback_editor.mode() == VimMode::Insert {
                Self::auto_wrap_vim_editor(&mut self.feedback_editor, wrap_width);
            }
        }
        Ok(false)
    }

    fn handle_delete_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.delete_mode_index = (self.delete_mode_index + 1) % 2;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.delete_mode_index = if self.delete_mode_index == 0 { 1 } else { 0 };
                }
                KeyCode::Enter => {
                    let mode = if self.delete_mode_index == 0 {
                        DeleteMode::Everything
                    } else {
                        DeleteMode::TaskOnly
                    };
                    self.delete_task(mode)?;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_restart_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.restart_confirm_index = (self.restart_confirm_index + 1) % 2;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.restart_confirm_index = if self.restart_confirm_index == 0 { 1 } else { 0 };
                }
                KeyCode::Enter => {
                    if self.restart_confirm_index == 0 {
                        // "Restart now"
                        self.should_restart = true;
                    } else {
                        // "Later"
                        self.view = View::TaskList;
                        self.set_status("Restart available — press U to restart".to_string());
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                    self.set_status("Restart available — press U to restart".to_string());
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
                WizardStep::SelectRepo => match key.code {
                    KeyCode::Esc => {
                        self.wizard = None;
                        self.view = View::TaskList;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let total = wizard.total_repo_count();
                        if total > 0 {
                            wizard.selected_repo_index =
                                (wizard.selected_repo_index + 1) % total;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let total = wizard.total_repo_count();
                        if total > 0 {
                            wizard.selected_repo_index = if wizard.selected_repo_index == 0 {
                                total - 1
                            } else {
                                wizard.selected_repo_index - 1
                            };
                        }
                    }
                    KeyCode::Enter => {
                        self.wizard_next_step()?;
                    }
                    _ => {}
                },
                WizardStep::SelectBranch => {
                    match key.code {
                        KeyCode::Esc => {
                            self.wizard_prev_step();
                        }
                        KeyCode::Tab => {
                            // Cycle forward: NewBranch → ExistingBranch → ExistingWorktree → NewBranch
                            wizard.branch_source = match wizard.branch_source {
                                BranchSource::NewBranch => BranchSource::ExistingBranch,
                                BranchSource::ExistingBranch => BranchSource::ExistingWorktree,
                                BranchSource::ExistingWorktree => BranchSource::NewBranch,
                            };
                        }
                        KeyCode::BackTab => {
                            // Cycle backward
                            wizard.branch_source = match wizard.branch_source {
                                BranchSource::NewBranch => BranchSource::ExistingWorktree,
                                BranchSource::ExistingBranch => BranchSource::NewBranch,
                                BranchSource::ExistingWorktree => BranchSource::ExistingBranch,
                            };
                        }
                        KeyCode::Enter => {
                            self.wizard_next_step()?;
                        }
                        _ => {
                            match wizard.branch_source {
                                BranchSource::NewBranch => {
                                    // Handle text input for branch name
                                    let input = Input::from(event.clone());
                                    wizard.new_branch_editor.input(input);
                                }
                                BranchSource::ExistingBranch => {
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
                                BranchSource::ExistingWorktree => {
                                    match key.code {
                                        KeyCode::Char('j') | KeyCode::Down => {
                                            if !wizard.existing_worktrees.is_empty() {
                                                wizard.selected_worktree_index =
                                                    (wizard.selected_worktree_index + 1)
                                                        % wizard.existing_worktrees.len();
                                            }
                                        }
                                        KeyCode::Char('k') | KeyCode::Up => {
                                            if !wizard.existing_worktrees.is_empty() {
                                                wizard.selected_worktree_index =
                                                    if wizard.selected_worktree_index == 0 {
                                                        wizard.existing_worktrees.len() - 1
                                                    } else {
                                                        wizard.selected_worktree_index - 1
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

                    // Check for Ctrl+S to submit in any mode
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('s')
                    {
                        wizard.description_editor.set_normal_mode();
                        self.wizard_next_step()?;
                        return Ok(false);
                    }

                    // Toggle review_after with Ctrl+R
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('r')
                    {
                        wizard.review_after = !wizard.review_after;
                        return Ok(false);
                    }

                    let input = Input::from(event.clone());
                    let was_insert = wizard.description_editor.mode() == VimMode::Insert;

                    wizard.description_editor.input(input.clone());

                    let is_normal_now = wizard.description_editor.mode() == VimMode::Normal;

                    // If we pressed Esc while already in normal mode, go back
                    if input.key == Key::Esc && !was_insert && is_normal_now {
                        self.wizard_prev_step();
                        return Ok(false);
                    }

                    // Auto-wrap only in insert mode
                    if wizard.description_editor.mode() == VimMode::Insert {
                        Self::auto_wrap_vim_editor(&mut wizard.description_editor, wrap_width);
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
                    self.view = View::Preview;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.commands.is_empty() {
                        self.selected_command_index =
                            (self.selected_command_index + 1) % self.commands.len();
                        self.command_list_state.select(Some(self.selected_command_index));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !self.commands.is_empty() {
                        self.selected_command_index = if self.selected_command_index == 0 {
                            self.commands.len() - 1
                        } else {
                            self.selected_command_index - 1
                        };
                        self.command_list_state.select(Some(self.selected_command_index));
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

    fn open_feedback_queue(&mut self) {
        if let Some(task) = self.selected_task() {
            if task.queued_feedback_count() == 0 {
                self.set_status("No feedback queued for this task".to_string());
                return;
            }
        } else {
            self.set_status("No task selected".to_string());
            return;
        }
        self.selected_queue_index = 0;
        self.view = View::FeedbackQueue;
    }

    fn handle_feedback_queue_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            let queue_len = self.selected_task()
                .map(|t| t.queued_feedback_count())
                .unwrap_or(0);

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::Preview;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if queue_len > 0 {
                        self.selected_queue_index =
                            (self.selected_queue_index + 1) % queue_len;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if queue_len > 0 {
                        self.selected_queue_index = if self.selected_queue_index == 0 {
                            queue_len - 1
                        } else {
                            self.selected_queue_index - 1
                        };
                    }
                }
                KeyCode::Char('d') | KeyCode::Delete => {
                    // Delete selected feedback item
                    self.delete_queued_feedback()?;
                }
                KeyCode::Char('C') => {
                    // Clear all queued feedback
                    self.clear_all_queued_feedback()?;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_rebase_branch_picker_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::Preview;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.rebase_branches.is_empty() {
                        self.selected_rebase_branch_index =
                            (self.selected_rebase_branch_index + 1) % self.rebase_branches.len();
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !self.rebase_branches.is_empty() {
                        self.selected_rebase_branch_index =
                            if self.selected_rebase_branch_index == 0 {
                                self.rebase_branches.len() - 1
                            } else {
                                self.selected_rebase_branch_index - 1
                            };
                    }
                }
                KeyCode::Enter => {
                    if let Some(branch) = self.rebase_branches.get(self.selected_rebase_branch_index).cloned() {
                        self.run_branch_command(&branch)?;
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_review_wizard_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            let wizard = match &mut self.review_wizard {
                Some(w) => w,
                None => {
                    self.view = View::TaskList;
                    return Ok(false);
                }
            };

            // Clear error on any keypress
            wizard.error_message = None;

            match wizard.step {
                ReviewWizardStep::SelectRepo => match key.code {
                    KeyCode::Esc => {
                        self.review_wizard = None;
                        self.view = View::TaskList;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let total = wizard.total_repo_count();
                        if total > 0 {
                            wizard.selected_repo_index =
                                (wizard.selected_repo_index + 1) % total;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        let total = wizard.total_repo_count();
                        if total > 0 {
                            wizard.selected_repo_index = if wizard.selected_repo_index == 0 {
                                total - 1
                            } else {
                                wizard.selected_repo_index - 1
                            };
                        }
                    }
                    KeyCode::Enter => {
                        self.review_wizard_next_step()?;
                    }
                    _ => {}
                },
                ReviewWizardStep::EnterBranch => {
                    match key.code {
                        KeyCode::Esc => {
                            self.review_wizard_prev_step();
                        }
                        KeyCode::Tab => {
                            wizard.branch_source = match wizard.branch_source {
                                BranchSource::NewBranch => BranchSource::ExistingBranch,
                                BranchSource::ExistingBranch => BranchSource::ExistingWorktree,
                                BranchSource::ExistingWorktree => BranchSource::NewBranch,
                            };
                        }
                        KeyCode::BackTab => {
                            wizard.branch_source = match wizard.branch_source {
                                BranchSource::NewBranch => BranchSource::ExistingWorktree,
                                BranchSource::ExistingBranch => BranchSource::NewBranch,
                                BranchSource::ExistingWorktree => BranchSource::ExistingBranch,
                            };
                        }
                        KeyCode::Enter => {
                            self.review_wizard_next_step()?;
                        }
                        _ => {
                            match wizard.branch_source {
                                BranchSource::NewBranch => {
                                    let input = Input::from(event.clone());
                                    wizard.branch_editor.input(input);
                                }
                                BranchSource::ExistingBranch => match key.code {
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
                                },
                                BranchSource::ExistingWorktree => match key.code {
                                    KeyCode::Char('j') | KeyCode::Down => {
                                        if !wizard.existing_worktrees.is_empty() {
                                            wizard.selected_worktree_index =
                                                (wizard.selected_worktree_index + 1)
                                                    % wizard.existing_worktrees.len();
                                        }
                                    }
                                    KeyCode::Char('k') | KeyCode::Up => {
                                        if !wizard.existing_worktrees.is_empty() {
                                            wizard.selected_worktree_index =
                                                if wizard.selected_worktree_index == 0 {
                                                    wizard.existing_worktrees.len() - 1
                                                } else {
                                                    wizard.selected_worktree_index - 1
                                                };
                                        }
                                    }
                                    _ => {}
                                },
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    fn delete_queued_feedback(&mut self) -> Result<()> {
        if let Some(task) = self.tasks.get(self.selected_index) {
            let queue_len = task.queued_feedback_count();
            if queue_len == 0 {
                return Ok(());
            }

            use_cases::delete_queued_feedback(task, self.selected_queue_index)?;

            // Adjust selected index if needed
            let remaining = task.queued_feedback_count();
            if self.selected_queue_index >= remaining && self.selected_queue_index > 0 {
                self.selected_queue_index -= 1;
            }

            if remaining == 0 {
                self.view = View::Preview;
                self.set_status("Queue cleared".to_string());
            } else {
                self.set_status(format!("Removed item ({} remaining)", remaining));
            }
        }
        Ok(())
    }

    fn clear_all_queued_feedback(&mut self) -> Result<()> {
        if let Some(task) = self.tasks.get_mut(self.selected_index) {
            use_cases::clear_all_queued_feedback(task)?;
            self.view = View::Preview;
            self.set_status("Queue cleared".to_string());
        }
        Ok(())
    }

    // === Restart Wizard Methods ===

    fn start_restart_wizard(&mut self) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        // Extract task info before mutable borrows
        let (task_id, status, flow_name, tmux_session, task_content) =
            match self.selected_task() {
                Some(t) => (
                    t.meta.task_id(),
                    t.meta.status,
                    t.meta.flow_name.clone(),
                    t.meta.tmux_session.clone(),
                    t.read_task().unwrap_or_else(|_| "No TASK.md available".to_string()),
                ),
                None => return Ok(()),
            };

        // If task is running, stop it first
        if status == TaskStatus::Running {
            // Inline stop logic (same as stop_task but we already have the info)
            if Tmux::session_exists(&tmux_session) {
                let _ = Tmux::send_ctrl_c_to_window(&tmux_session, "agman");
            }
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                let _ = task.update_status(TaskStatus::Stopped);
                task.meta.current_agent = None;
                let _ = task.save_meta();
            }
            tracing::info!(task_id = %task_id, old_status = "running", new_status = "stopped", "stopped task before restart");
            self.log_output(format!("Stopped {} before restart", task_id));
        }

        // Load flow to enumerate steps
        let flow_path = self.config.flow_path(&flow_name);
        let flow = match Flow::load(&flow_path) {
            Ok(f) => f,
            Err(e) => {
                self.set_status(format!("Failed to load flow '{}': {}", flow_name, e));
                return Ok(());
            }
        };

        let flow_steps: Vec<String> = flow
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| s.display_label(i))
            .collect();

        if flow_steps.is_empty() {
            self.set_status("Flow has no steps".to_string());
            return Ok(());
        }

        // Pre-wrap content for the editor
        let wrap_width = crossterm::terminal::size()
            .map(|(w, _)| ((w as f32 * 0.80) as usize).saturating_sub(6))
            .unwrap_or(70);
        let wrapped = Self::wrap_content(&task_content, wrap_width);
        let task_editor = VimTextArea::from_lines(wrapped.lines());

        // Load preview if coming from TaskList
        if self.view == View::TaskList {
            self.load_preview();
        }

        self.restart_wizard = Some(RestartWizard {
            step: RestartWizardStep::EditTask,
            task_editor,
            flow_steps,
            selected_step_index: 0,
            task_id,
        });

        self.view = View::RestartWizard;
        Ok(())
    }

    fn handle_restart_wizard_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            let wizard_step = match &self.restart_wizard {
                Some(w) => w.step,
                None => {
                    self.view = View::Preview;
                    return Ok(false);
                }
            };

            match wizard_step {
                RestartWizardStep::EditTask => {
                    // Calculate wrap width
                    let wrap_width = crossterm::terminal::size()
                        .map(|(w, _)| ((w as f32 * 0.80) as usize).saturating_sub(6))
                        .unwrap_or(70);

                    // Ctrl+S: save TASK.md and advance to SelectAgent
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('s')
                    {
                        // Save TASK.md
                        let content = self
                            .restart_wizard
                            .as_ref()
                            .map(|w| w.task_editor.lines_joined())
                            .unwrap_or_default();
                        let task_id = self
                            .restart_wizard
                            .as_ref()
                            .map(|w| w.task_id.clone())
                            .unwrap_or_default();
                        if let Some(task) = self.tasks.iter().find(|t| t.meta.task_id() == task_id)
                        {
                            let _ = task.write_task(&content);
                        }
                        self.set_status("TASK.md saved".to_string());

                        if let Some(w) = &mut self.restart_wizard {
                            w.task_editor.set_normal_mode();
                            w.step = RestartWizardStep::SelectAgent;
                        }
                        return Ok(false);
                    }

                    // Tab: skip to SelectAgent without saving
                    if key.code == KeyCode::Tab {
                        if let Some(w) = &mut self.restart_wizard {
                            w.task_editor.set_normal_mode();
                            w.step = RestartWizardStep::SelectAgent;
                        }
                        return Ok(false);
                    }

                    let input = Input::from(event.clone());
                    let was_insert = self
                        .restart_wizard
                        .as_ref()
                        .map(|w| w.task_editor.mode() == VimMode::Insert)
                        .unwrap_or(false);

                    if let Some(w) = &mut self.restart_wizard {
                        w.task_editor.input(input.clone());
                    }

                    let is_normal_now = self
                        .restart_wizard
                        .as_ref()
                        .map(|w| w.task_editor.mode() == VimMode::Normal)
                        .unwrap_or(false);

                    // Esc in normal mode → cancel wizard
                    if input.key == Key::Esc && !was_insert && is_normal_now {
                        self.restart_wizard = None;
                        self.view = View::Preview;
                        self.set_status("Restart cancelled".to_string());
                        return Ok(false);
                    }

                    // Auto-wrap in insert mode
                    if let Some(w) = &mut self.restart_wizard {
                        if w.task_editor.mode() == VimMode::Insert {
                            Self::auto_wrap_vim_editor(&mut w.task_editor, wrap_width);
                        }
                    }
                }
                RestartWizardStep::SelectAgent => match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Some(w) = &mut self.restart_wizard {
                            let max = w.flow_steps.len().saturating_sub(1);
                            if w.selected_step_index < max {
                                w.selected_step_index += 1;
                            }
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Some(w) = &mut self.restart_wizard {
                            if w.selected_step_index > 0 {
                                w.selected_step_index -= 1;
                            }
                        }
                    }
                    KeyCode::Enter => {
                        self.execute_restart_wizard()?;
                    }
                    KeyCode::Esc => {
                        // Go back to EditTask step
                        if let Some(w) = &mut self.restart_wizard {
                            w.step = RestartWizardStep::EditTask;
                        }
                    }
                    _ => {}
                },
            }
        }
        Ok(false)
    }

    fn handle_set_linked_pr_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            match key.code {
                KeyCode::Esc => {
                    self.view = View::TaskList;
                }
                KeyCode::Tab => {
                    self.pr_owned_toggle = !self.pr_owned_toggle;
                }
                KeyCode::Enter => {
                    let text: String = self.pr_number_editor.lines().join("");
                    let text = text.trim().to_string();

                    if text.is_empty() {
                        // Clear linked PR
                        let task_id_for_log = self.selected_task().map(|t| t.meta.task_id());
                        tracing::info!(task_id = ?task_id_for_log, "TUI: clear linked PR");
                        if let Some(task) = self.tasks.get_mut(self.selected_index) {
                            match use_cases::clear_linked_pr(task) {
                                Ok(()) => self.set_status("PR link cleared".to_string()),
                                Err(e) => self.set_status(format!("Error: {}", e)),
                            }
                        }
                    } else {
                        // Parse as number and set
                        match text.parse::<u64>() {
                            Ok(pr_number) => {
                                let task_id_for_log = self.selected_task().map(|t| t.meta.task_id());
                                let owned = self.pr_owned_toggle;
                                tracing::info!(task_id = ?task_id_for_log, pr_number, owned, "TUI: set linked PR");
                                let worktree_path = self
                                    .selected_task()
                                    .map(|t| t.meta.worktree_path.clone());
                                if let Some(wt) = worktree_path {
                                    if let Some(task) =
                                        self.tasks.get_mut(self.selected_index)
                                    {
                                        match use_cases::set_linked_pr(task, pr_number, &wt, owned) {
                                            Ok(()) => {
                                                let label = if owned { "mine" } else { "ext" };
                                                self.set_status(format!(
                                                    "Linked PR #{} ({})",
                                                    pr_number, label
                                                ));
                                            }
                                            Err(e) => {
                                                self.set_status(format!("Error: {}", e))
                                            }
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                self.set_status("Invalid PR number".to_string());
                            }
                        }
                    }
                    self.view = View::TaskList;
                }
                _ => {
                    self.pr_number_editor.input(Input::from(event));
                }
            }
        }
        Ok(false)
    }

    fn handle_directory_picker_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    let origin = self.dir_picker.as_ref().map(|p| p.origin);
                    self.dir_picker = None;
                    self.view = View::TaskList;
                    // If cancelled, show a helpful message
                    match origin {
                        Some(DirPickerOrigin::NewTask) | Some(DirPickerOrigin::Review) => {
                            self.set_status("Directory selection cancelled".to_string());
                        }
                        None => {}
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(picker) = &mut self.dir_picker {
                        if !picker.entries.is_empty() {
                            picker.selected_index =
                                (picker.selected_index + 1) % picker.entries.len();
                        }
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(picker) = &mut self.dir_picker {
                        if !picker.entries.is_empty() {
                            picker.selected_index = if picker.selected_index == 0 {
                                picker.entries.len() - 1
                            } else {
                                picker.selected_index - 1
                            };
                        }
                    }
                }
                KeyCode::Char('l') | KeyCode::Enter => {
                    // Enter selected directory
                    if let Some(picker) = &mut self.dir_picker {
                        picker.enter_selected();
                    }
                }
                KeyCode::Char('h') | KeyCode::Backspace => {
                    // Go up one directory
                    if let Some(picker) = &mut self.dir_picker {
                        picker.go_up();
                    }
                }
                KeyCode::Char('s') => {
                    // Select current directory as repos_dir
                    if let Some(picker) = self.dir_picker.take() {
                        let selected_dir = picker.current_dir.clone();
                        let origin = picker.origin;

                        // Save to config.toml
                        let config_file = agman::config::ConfigFile {
                            repos_dir: Some(selected_dir.to_string_lossy().to_string()),
                        };
                        if let Err(e) = agman::config::save_config_file(&self.config.base_dir, &config_file) {
                            self.set_status(format!("Failed to save config: {}", e));
                            self.view = View::TaskList;
                            return Ok(false);
                        }

                        // Update in-memory config
                        self.config.repos_dir = selected_dir;
                        tracing::info!(repos_dir = %self.config.repos_dir.display(), "repos_dir updated via directory picker");

                        // Resume the original wizard
                        match origin {
                            DirPickerOrigin::NewTask => {
                                self.start_wizard()?;
                            }
                            DirPickerOrigin::Review => {
                                self.start_review_wizard()?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Spawn a background task to poll linked PRs for all eligible tasks.
    /// Uses tokio's spawn_blocking to run `gh pr view` calls off the main thread
    /// and sends results back via the channel so the TUI stays responsive.
    fn start_pr_poll(&mut self) {
        if self.pr_poll_active {
            return;
        }

        let eligible: Vec<(String, u64, PathBuf, Option<u64>)> = self
            .tasks
            .iter()
            .filter(|t| t.meta.status == TaskStatus::Stopped && t.meta.linked_pr.as_ref().is_some_and(|pr| pr.owned))
            .map(|t| {
                let pr = t.meta.linked_pr.as_ref().unwrap();
                (
                    t.meta.task_id(),
                    pr.number,
                    t.meta.worktree_path.clone(),
                    t.meta.last_review_count,
                )
            })
            .collect();

        if eligible.is_empty() {
            return;
        }

        self.pr_poll_active = true;
        let tx = self.pr_poll_tx.clone();

        self.rt.spawn(async move {
            let results = tokio::task::spawn_blocking(move || run_pr_queries(eligible))
                .await
                .unwrap_or_default();
            let _ = tx.send(results);
        });
    }

    /// Check for completed PR poll results (non-blocking) and apply actions.
    fn apply_pr_poll_results(&mut self) {
        let results = match self.pr_poll_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        self.pr_poll_active = false;

        // First: handle non-delete actions
        for result in &results {
            match &result.action {
                use_cases::PrPollAction::AddressReview { .. } => {
                    if let Some(task) = self
                        .tasks
                        .iter_mut()
                        .find(|t| t.meta.task_id() == result.task_id)
                    {
                        let _ = use_cases::update_last_review_count(task, result.review_count);
                        let _ = use_cases::set_review_addressed(task, false);
                    }

                    let output = Command::new("agman")
                        .args(["run-command", &result.task_id, "address-review"])
                        .output();

                    match output {
                        Ok(o) if o.status.success() => {
                            self.log_output(format!(
                                "Auto-triggered address-review for {}: new review on PR #{}",
                                result.task_id, result.pr_number
                            ));
                            if let Some(task) = self
                                .tasks
                                .iter_mut()
                                .find(|t| t.meta.task_id() == result.task_id)
                            {
                                let _ = use_cases::set_review_addressed(task, true);
                            }
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            self.log_output(format!(
                                "Failed to auto-trigger address-review for {}: {}",
                                result.task_id, stderr
                            ));
                        }
                        Err(e) => {
                            self.log_output(format!(
                                "Error triggering address-review for {}: {}",
                                result.task_id, e
                            ));
                        }
                    }
                }
                use_cases::PrPollAction::None => {
                    if let Some(task) = self
                        .tasks
                        .iter_mut()
                        .find(|t| t.meta.task_id() == result.task_id)
                    {
                        if task.meta.last_review_count.is_none() {
                            let _ =
                                use_cases::update_last_review_count(task, result.review_count);
                        }
                    }
                }
                use_cases::PrPollAction::DeleteMerged => {} // handled below
            }
        }

        // Then: handle deletions
        let to_delete: Vec<(String, u64)> = results
            .iter()
            .filter(|r| matches!(r.action, use_cases::PrPollAction::DeleteMerged))
            .map(|r| (r.task_id.clone(), r.pr_number))
            .collect();

        for (task_id, pr_number) in to_delete {
            if let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == task_id) {
                let task = self.tasks.remove(idx);
                let tmux_session = task.meta.tmux_session.clone();

                let _ = Tmux::kill_session(&tmux_session);
                let _ = use_cases::delete_task(
                    &self.config,
                    task,
                    use_cases::DeleteMode::Everything,
                );

                self.log_output(format!(
                    "Auto-deleted task {}: PR #{} merged",
                    task_id, pr_number
                ));

                if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
                    self.selected_index = self.tasks.len() - 1;
                }
            }
        }
    }

    fn execute_restart_wizard(&mut self) -> Result<()> {
        let (task_id, selected_step_index) = match &self.restart_wizard {
            Some(w) => (w.task_id.clone(), w.selected_step_index),
            None => return Ok(()),
        };

        tracing::info!(task_id = %task_id, step = selected_step_index, "TUI: restart task from wizard");
        // Find the task and update it
        let task_idx = match self.tasks.iter().position(|t| t.meta.task_id() == task_id) {
            Some(i) => i,
            None => {
                self.set_status(format!("Task {} not found", task_id));
                self.restart_wizard = None;
                self.view = View::TaskList;
                return Ok(());
            }
        };

        let tmux_session = self.tasks[task_idx].meta.tmux_session.clone();
        let worktree_path = self.tasks[task_idx].meta.worktree_path.clone();

        // Set flow_step and status
        use_cases::restart_task(&mut self.tasks[task_idx], selected_step_index)?;
        // Clear review_addressed on restart
        let _ = use_cases::set_review_addressed(&mut self.tasks[task_idx], false);

        // Ensure tmux session exists
        if !Tmux::session_exists(&tmux_session) {
            let _ = Tmux::create_session_with_windows(&tmux_session, &worktree_path);
            let _ = Tmux::add_review_window(&tmux_session, &worktree_path);
        }

        // Dispatch flow-run
        let flow_cmd = format!("agman flow-run {}", task_id);
        let _ = Tmux::send_keys_to_window(&tmux_session, "agman", &flow_cmd);

        // Clean up wizard
        self.restart_wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id)?;
        self.set_status(format!("Restarted: {} from step {}", task_id, selected_step_index));

        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_caffeinate();
    }
}

pub fn run_tui(config: Config) -> Result<()> {
    // Remove any stale restart signal file left over from a previous build.
    // This prevents a "double restart" if the TUI missed the signal (e.g. it
    // crashed or was not running when release.sh created the file).
    #[cfg(unix)]
    {
        let restart_signal = dirs::home_dir()
            .unwrap_or_default()
            .join(".agman/.restart-tui");
        if restart_signal.exists() {
            tracing::info!("removing stale .restart-tui signal file at startup");
            let _ = std::fs::remove_file(&restart_signal);
        }
    }

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

                if app.should_quit || app.should_restart {
                    break;
                }
            }

            // Periodic refresh of task list (every 3 seconds, only in TaskList view)
            if last_refresh.elapsed() >= refresh_interval {
                if app.view == View::TaskList {
                    let _ = app.refresh_tasks();
                    // Check for stranded feedback queues on stopped tasks
                    app.process_stranded_feedback();
                }
                last_refresh = Instant::now();
            }

            // Poll linked PRs every 60 seconds (regardless of view)
            if app.last_pr_poll.elapsed() >= Duration::from_secs(60) {
                app.start_pr_poll();
                app.last_pr_poll = Instant::now();
            }

            // Check for completed background PR poll results (non-blocking)
            app.apply_pr_poll_results();

            // Check for restart signal (written by release.sh when agman is rebuilt)
            #[cfg(unix)]
            {
                let restart_signal = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".agman/.restart-tui");
                if restart_signal.exists() {
                    tracing::info!("detected .restart-tui signal file, deferring to restart modal");
                    let _ = std::fs::remove_file(&restart_signal);
                    app.restart_pending = true;
                }
            }

            // Show restart modal when no other modal is active
            if app.restart_pending
                && matches!(app.view, View::TaskList | View::Preview)
            {
                app.restart_confirm_index = 0;
                app.view = View::RestartConfirm;
                app.restart_pending = false;
            }

            // Clear old status messages
            app.clear_old_status();
            app.clear_old_output();
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        // If user confirmed restart, exec the new binary
        #[cfg(unix)]
        if app.should_restart {
            app.stop_caffeinate();
            eprintln!("agman updated — restarting...");
            let err = Command::new("agman").exec();
            // exec only returns on error
            eprintln!("Failed to restart: {err}");
            std::process::exit(1);
        }

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
