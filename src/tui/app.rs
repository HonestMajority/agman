use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, widgets::ListState, Terminal};
use std::collections::HashSet;
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
use agman::dismissed_notifications::DismissedNotifications;
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
    SessionPicker,
    Notifications,
    Notes,
}

/// Which wizard requested the directory picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirPickerOrigin {
    /// Fallback: no repos found, pick a repos_dir.
    NewTask,
    /// Fallback for review wizard: no repos found, pick a repos_dir.
    Review,
    /// Repo selection: browse directories to choose a repo or multi-repo parent.
    RepoSelect,
    /// Review repo selection: like RepoSelect but only allows single git repos (not multi-repo parents).
    ReviewRepoSelect,
}

/// Re-export DirKind for use in the picker UI.
pub use agman::use_cases::DirKind;

pub struct DirectoryPicker {
    pub current_dir: PathBuf,
    pub entries: Vec<String>,
    /// Classification of each entry (parallel to `entries`). Only populated for `RepoSelect`/`ReviewRepoSelect` origins.
    pub entry_kinds: Vec<DirKind>,
    pub selected_index: usize,
    pub origin: DirPickerOrigin,
    /// Favourite repos with task counts (loaded once at construction). Always shown in repo select modes.
    pub favorite_repos: Vec<(String, u64)>,
    /// The configured repos_dir, used to resolve favourite repo paths.
    pub repos_dir: PathBuf,
}

impl DirectoryPicker {
    fn new(start_dir: PathBuf, origin: DirPickerOrigin) -> Self {
        let mut picker = Self {
            current_dir: start_dir,
            entries: Vec::new(),
            entry_kinds: Vec::new(),
            selected_index: 0,
            origin,
            favorite_repos: Vec::new(),
            repos_dir: PathBuf::new(),
        };
        picker.refresh_entries();
        picker
    }

    fn new_with_favorites(
        start_dir: PathBuf,
        origin: DirPickerOrigin,
        stats_path: &std::path::Path,
        repos_dir: PathBuf,
    ) -> Self {
        let stats = RepoStats::load(stats_path);
        let favorite_repos: Vec<(String, u64)> = stats
            .favorites()
            .into_iter()
            .filter(|(name, _)| repos_dir.join(name).join(".git").exists())
            .collect();

        let mut picker = Self {
            current_dir: start_dir,
            entries: Vec::new(),
            entry_kinds: Vec::new(),
            selected_index: 0,
            origin,
            favorite_repos,
            repos_dir,
        };
        picker.refresh_entries();
        picker
    }

    pub fn is_repo_select_mode(&self) -> bool {
        matches!(self.origin, DirPickerOrigin::RepoSelect | DirPickerOrigin::ReviewRepoSelect)
    }

    fn refresh_entries(&mut self) {
        self.entries.clear();
        self.entry_kinds.clear();
        if let Ok(read_dir) = std::fs::read_dir(&self.current_dir) {
            let is_repo_select = self.is_repo_select_mode();
            let mut dirs: Vec<(String, PathBuf)> = read_dir
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter(|e| {
                    !e.file_name()
                        .to_string_lossy()
                        .starts_with('.')
                })
                .filter(|e| {
                    // In RepoSelect modes, filter out -wt worktree directories
                    if is_repo_select {
                        !e.file_name().to_string_lossy().ends_with("-wt")
                    } else {
                        true
                    }
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let path = e.path();
                    (name, path)
                })
                .collect();
            dirs.sort_by(|a, b| a.0.cmp(&b.0));

            for (name, path) in dirs {
                if is_repo_select {
                    self.entry_kinds.push(use_cases::classify_directory(&path));
                }
                self.entries.push(name);
            }
        }
        self.selected_index = 0;
    }

    /// Number of favourite entries visible (always shown when favourites are loaded).
    pub fn favorites_len(&self) -> usize {
        self.favorite_repos.len()
    }

    /// Total number of selectable items (favourites + directory entries).
    pub fn total_items(&self) -> usize {
        self.favorites_len() + self.entries.len()
    }

    fn enter_selected(&mut self) {
        let fav_len = self.favorites_len();
        if self.selected_index < fav_len {
            // Favourites are handled externally via select_repo_from_picker
            return;
        }
        let entry_idx = self.selected_index - fav_len;
        if let Some(name) = self.entries.get(entry_idx) {
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

    /// Get the kind of the currently selected entry (for RepoSelect mode).
    pub fn selected_entry_kind(&self) -> Option<DirKind> {
        let fav_len = self.favorites_len();
        if self.selected_index < fav_len {
            return Some(DirKind::GitRepo);
        }
        self.entry_kinds.get(self.selected_index - fav_len).copied()
    }

    /// Get the full path to the currently selected entry.
    pub fn selected_path(&self) -> Option<PathBuf> {
        let fav_len = self.favorites_len();
        if self.selected_index < fav_len {
            return Some(self.repos_dir.join(&self.favorite_repos[self.selected_index].0));
        }
        let entry_idx = self.selected_index - fav_len;
        self.entries
            .get(entry_idx)
            .map(|name| self.current_dir.join(name))
    }

    /// Get the name of the currently selected entry.
    pub fn selected_name(&self) -> Option<String> {
        let fav_len = self.favorites_len();
        if self.selected_index < fav_len {
            return Some(self.favorite_repos[self.selected_index].0.clone());
        }
        let entry_idx = self.selected_index - fav_len;
        self.entries.get(entry_idx).cloned()
    }

    /// Whether the currently selected item is a favourite.
    pub fn is_favorite_selected(&self) -> bool {
        self.selected_index < self.favorites_len()
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
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
    /// The selected repo name (directory basename).
    pub selected_repo: String,
    /// The full path to the selected repo/parent directory.
    pub selected_repo_path: PathBuf,
    pub branch_source: BranchSource,
    pub existing_worktrees: Vec<(String, PathBuf)>,
    pub selected_worktree_index: usize,
    pub existing_branches: Vec<String>,
    pub selected_branch_index: usize,
    pub new_branch_editor: TextArea<'static>,
    pub base_branch_editor: TextArea<'static>,
    /// Which field has focus in the NewBranch tab: false = branch name, true = base branch.
    pub base_branch_focus: bool,
    pub description_editor: VimTextArea<'static>,
    pub error_message: Option<String>,
    pub review_after: bool,
    /// True when a multi-repo parent directory was selected (not a git repo).
    pub is_multi_repo: bool,
}

impl NewTaskWizard {
    /// The selected repo name.
    pub fn selected_repo_name(&self) -> &str {
        &self.selected_repo
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewWizardStep {
    EnterBranch,
}

pub struct ReviewWizard {
    pub step: ReviewWizardStep,
    /// The repo name, selected via DirectoryPicker before creating the wizard.
    pub selected_repo: String,
    pub branch_source: BranchSource,
    pub branch_editor: TextArea<'static>,
    pub existing_branches: Vec<String>,
    pub selected_branch_index: usize,
    pub existing_worktrees: Vec<(String, PathBuf)>,
    pub selected_worktree_index: usize,
    pub error_message: Option<String>,
}

impl ReviewWizard {
    pub fn selected_repo_name(&self) -> &str {
        &self.selected_repo
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

/// Fetch the author login for a PR via `gh pr view`.
/// Returns `None` on any error so linking gracefully falls back.
fn fetch_pr_author(worktree_path: &std::path::Path, pr_number: u64) -> Option<String> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "author",
        ])
        .current_dir(worktree_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("author")?
        .get("login")?
        .as_str()
        .map(|s| s.to_string())
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

// ---------------------------------------------------------------------------
// Notes view state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesFocus {
    Explorer,
    Editor,
}

pub struct NotesView {
    pub root_dir: PathBuf,
    pub current_dir: PathBuf,
    pub entries: Vec<use_cases::NoteEntry>,
    pub selected_index: usize,
    pub focus: NotesFocus,
    pub editor: VimTextArea<'static>,
    pub open_file: Option<PathBuf>,
    pub modified: bool,
    pub rename_input: Option<TextArea<'static>>,
    /// (TextArea, is_dir) — inline input for creating a new note or directory.
    pub create_input: Option<(TextArea<'static>, bool)>,
    pub confirm_delete: bool,
    /// Cut state: `(source_dir, file_name)` of the entry being moved.
    pub cut_entry: Option<(PathBuf, String)>,
}

impl NotesView {
    pub fn new(root_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root_dir)?;
        let entries = use_cases::list_notes(&root_dir)?;
        Ok(Self {
            current_dir: root_dir.clone(),
            root_dir,
            entries,
            selected_index: 0,
            focus: NotesFocus::Explorer,
            editor: VimTextArea::new(),
            open_file: None,
            modified: false,
            rename_input: None,
            create_input: None,
            confirm_delete: false,
            cut_entry: None,
        })
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.entries = use_cases::list_notes(&self.current_dir)?;
        if self.selected_index >= self.entries.len() && !self.entries.is_empty() {
            self.selected_index = self.entries.len() - 1;
        }
        Ok(())
    }

    pub fn open_file(&mut self, path: &std::path::Path) -> Result<()> {
        let content = use_cases::read_note(path)?;
        self.editor = VimTextArea::from_lines(content.lines());
        self.open_file = Some(path.to_path_buf());
        self.modified = false;
        Ok(())
    }

    pub fn save_current(&mut self) -> Result<()> {
        if self.modified {
            if let Some(ref path) = self.open_file {
                let content = self.editor.lines_joined();
                use_cases::save_note(path, &content)?;
                self.modified = false;
            }
        }
        Ok(())
    }
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
    // Session picker for multi-repo attach
    pub session_picker_sessions: Vec<(String, String)>, // (repo_name, tmux_session)
    pub selected_session_index: usize,
    pub attach_session_name: Option<String>,
    // GitHub notifications polling
    pub notifications: Vec<use_cases::GithubNotification>,
    pub selected_notif_index: usize,
    pub last_gh_notif_poll: Instant,
    gh_notif_tx: tokio_mpsc::UnboundedSender<use_cases::NotifPollResult>,
    gh_notif_rx: tokio_mpsc::UnboundedReceiver<use_cases::NotifPollResult>,
    gh_notif_poll_active: bool,
    pub gh_notif_first_poll_done: bool,
    /// Thread IDs dismissed by the user, persisted across restarts.
    dismissed_notifs: DismissedNotifications,
    // Notes view
    pub notes_view: Option<NotesView>,
    // Sleep inhibition (macOS: caffeinate -dis for idle, display, and system sleep assertions)
    #[cfg(target_os = "macos")]
    caffeinate_process: Option<std::process::Child>,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        use_cases::migrate_old_tasks(&config);
        let tasks = Task::list_all(&config);
        let commands = StoredCommand::list_all(&config.commands_dir).unwrap_or_default();
        let notes_editor = VimTextArea::new();
        let feedback_editor = VimTextArea::new();
        let task_file_editor = VimTextArea::new();
        let (pr_poll_tx, pr_poll_rx) = tokio_mpsc::unbounded_channel();
        let (gh_notif_tx, gh_notif_rx) = tokio_mpsc::unbounded_channel();
        let rt = tokio::runtime::Runtime::new()?;
        let mut dismissed_notifs = DismissedNotifications::load(&config.dismissed_notifications_path());
        let retention = chrono::Duration::weeks(agman::dismissed_notifications::NOTIFICATION_RETENTION_WEEKS);
        if dismissed_notifs.prune_older_than(retention) > 0 {
            dismissed_notifs.save(&config.dismissed_notifications_path());
        }

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
            session_picker_sessions: Vec::new(),
            selected_session_index: 0,
            attach_session_name: None,
            notifications: Vec::new(),
            selected_notif_index: 0,
            last_gh_notif_poll: Instant::now() - Duration::from_secs(60),
            gh_notif_tx,
            gh_notif_rx,
            gh_notif_poll_active: false,
            gh_notif_first_poll_done: false,
            dismissed_notifs,
            notes_view: None,
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
            if line.chars().count() <= max_width {
                result.push(line.to_string());
            } else {
                let mut remaining = line;
                while remaining.chars().count() > max_width {
                    // Find the byte offset of the max_width-th character
                    let byte_offset = remaining
                        .char_indices()
                        .nth(max_width)
                        .map(|(i, _)| i)
                        .unwrap_or(remaining.len());
                    let wrap_at = remaining[..byte_offset]
                        .rfind(' ')
                        .unwrap_or(byte_offset);
                    if wrap_at == 0 {
                        // No space found, force break at char boundary
                        result.push(remaining[..byte_offset].to_string());
                        remaining = &remaining[byte_offset..];
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

    pub fn refresh_tasks(&mut self) {
        let prev_task_id = self.selected_task().map(|t| t.meta.task_id());
        self.tasks = Task::list_all(&self.config);
        if let Some(ref id) = prev_task_id {
            if let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == *id) {
                self.selected_index = idx;
                return;
            }
        }
        if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_index = self.tasks.len() - 1;
        }
    }

    /// Refresh the task list and restore selection to the task with the given ID.
    /// If the task is no longer present, selection falls back to a valid index.
    fn refresh_tasks_and_select(&mut self, task_id: &str) {
        self.refresh_tasks();
        if let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == task_id) {
            self.selected_index = idx;
        }
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
        let task_info = self.selected_task().and_then(|t| {
            // For multi-repo tasks without repos, use the parent session name
            let tmux_session = if t.meta.has_repos() {
                t.meta.primary_repo().tmux_session.clone()
            } else if t.meta.is_multi_repo() {
                Config::tmux_session_name(&t.meta.name, &t.meta.branch_name)
            } else {
                return None;
            };
            Some((t.meta.task_id(), tmux_session, t.meta.status))
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
                    self.refresh_tasks_and_select(&task_id);
                }
                TaskStatus::OnHold => {
                    tracing::info!(task_id = %task_id, "TUI: resume from hold requested");
                    if let Some(task) = self.tasks.get_mut(self.selected_index) {
                        use_cases::resume_from_hold(task)?;
                    }
                    self.set_status(format!("Resumed: {}", task_id));
                    self.refresh_tasks_and_select(&task_id);
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
        let task_info = self.selected_task().and_then(|t| {
            let tmux_session = if t.meta.has_repos() {
                t.meta.primary_repo().tmux_session.clone()
            } else if t.meta.is_multi_repo() {
                Config::tmux_session_name(&t.meta.name, &t.meta.branch_name)
            } else {
                return None;
            };
            Some((t.meta.task_id(), tmux_session, t.meta.status))
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

            // Side effects: ensure tmux sessions exist for all repos and dispatch flow
            if let Some(task) = self.selected_task() {
                for repo in &task.meta.repos {
                    if !Tmux::session_exists(&repo.tmux_session) {
                        let _ = Tmux::create_session_with_windows(&repo.tmux_session, &repo.worktree_path);
                        let _ = Tmux::add_review_window(&repo.tmux_session, &repo.worktree_path);
                    }
                }
                // For multi-repo tasks with no repos yet, ensure the primary session exists
                if !task.meta.has_repos() && !Tmux::session_exists(&tmux_session) {
                    if let Some(ref parent) = task.meta.parent_dir {
                        let _ = Tmux::create_session_with_windows(&tmux_session, parent);
                    }
                }
            }

            let flow_cmd = format!("agman flow-run {}", task_id);
            let _ = Tmux::send_keys_to_window(&tmux_session, "agman", &flow_cmd);

            self.log_output(format!("Resumed flow for {} — processing your answers", task_id));
            self.set_status(format!("Resumed: {}", task_id));
            self.refresh_tasks_and_select(&task_id);
        }

        Ok(())
    }

    fn delete_task(&mut self, mode: DeleteMode) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let task = self.tasks.remove(self.selected_index);
        let task_id = task.meta.task_id();

        let uc_mode = match mode {
            DeleteMode::Everything => use_cases::DeleteMode::Everything,
            DeleteMode::TaskOnly => use_cases::DeleteMode::TaskOnly,
        };

        tracing::info!(task_id = %task_id, mode = ?mode, "TUI: delete task requested");
        self.log_output(format!("Deleting task {} ({})...", task_id, match mode {
            DeleteMode::Everything => "everything",
            DeleteMode::TaskOnly => "task only",
        }));

        // Kill tmux sessions for all repos (side effect)
        if task.meta.has_repos() {
            for repo in &task.meta.repos {
                let _ = Tmux::kill_session(&repo.tmux_session);
            }
        }
        // Also kill the parent-dir session (used for repo-inspector in multi-repo tasks)
        if task.meta.is_multi_repo() {
            let parent_session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
            let _ = Tmux::kill_session(&parent_session);
        }
        self.log_output("  Killed tmux session(s)".to_string());

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
                    self.refresh_tasks_and_select(&task_id);
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
        let start = if self.config.repos_dir.exists() {
            self.config.repos_dir.clone()
        } else {
            // No repos_dir configured — use fallback picker to set repos_dir first
            let start = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            self.dir_picker = Some(DirectoryPicker::new(start, DirPickerOrigin::NewTask));
            self.view = View::DirectoryPicker;
            self.set_status(format!("No repos found in {}. Pick a repos directory (s to select, h/l to navigate).", self.config.repos_dir.display()));
            return Ok(());
        };

        // Launch the directory picker in RepoSelect mode, rooted at repos_dir, with favourites
        self.dir_picker = Some(DirectoryPicker::new_with_favorites(
            start,
            DirPickerOrigin::RepoSelect,
            &self.config.repo_stats_path(),
            self.config.repos_dir.clone(),
        ));
        self.view = View::DirectoryPicker;
        Ok(())
    }

    /// Create the wizard from a directory picker selection, starting at `SelectBranch`.
    fn create_wizard_from_picker(&mut self, repo_name: String, repo_path: PathBuf, is_multi: bool) -> Result<()> {
        let mut new_branch_editor = Self::create_plain_editor();
        new_branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        let mut base_branch_editor = Self::create_plain_editor();
        base_branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        // Pre-fill base branch editor with auto-detected ref
        if is_multi {
            base_branch_editor.insert_str("origin/main");
        } else {
            let base_ref = Git::find_base_ref(&repo_path);
            base_branch_editor.insert_str(&base_ref);
        }

        // Description editor uses vim mode, start in insert mode
        let mut description_editor = VimTextArea::new();
        description_editor.set_insert_mode();

        let (branches, worktrees) = if is_multi {
            (Vec::new(), Vec::new())
        } else {
            let branches = self.scan_branches(&repo_name)?;
            let worktrees = self.scan_existing_worktrees(&repo_name)?;
            (branches, worktrees)
        };

        self.wizard = Some(NewTaskWizard {
            step: WizardStep::SelectBranch,
            selected_repo: repo_name,
            selected_repo_path: repo_path,
            branch_source: BranchSource::NewBranch,
            existing_worktrees: worktrees,
            selected_worktree_index: 0,
            existing_branches: branches,
            selected_branch_index: 0,
            new_branch_editor,
            base_branch_editor,
            base_branch_focus: false,
            description_editor,
            error_message: None,
            review_after: false,
            is_multi_repo: is_multi,
        });

        self.view = View::NewTaskWizard;
        Ok(())
    }

    /// Create a ReviewWizard from a directory picker selection, starting at `EnterBranch`.
    fn create_review_wizard_from_picker(&mut self, repo_name: String) -> Result<()> {
        let branches = self.scan_branches(&repo_name)?;
        let worktrees = self.scan_existing_worktrees(&repo_name)?;

        let mut branch_editor = Self::create_plain_editor();
        branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        self.review_wizard = Some(ReviewWizard {
            step: ReviewWizardStep::EnterBranch,
            selected_repo: repo_name,
            branch_source: BranchSource::NewBranch,
            branch_editor,
            existing_branches: branches,
            selected_branch_index: 0,
            existing_worktrees: worktrees,
            selected_worktree_index: 0,
            error_message: None,
        });

        self.view = View::ReviewWizard;
        Ok(())
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
            Some(t) => {
                if t.meta.is_multi_repo() {
                    self.set_status("Branch picker not supported for multi-repo tasks".to_string());
                    return;
                }
                if !t.meta.has_repos() {
                    self.set_status("Task has no repos configured yet".to_string());
                    return;
                }
                t.meta.primary_repo().repo_name.clone()
            }
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
                self.refresh_tasks_and_select(&task_id);
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
                self.refresh_tasks_and_select(&task_id);
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
            .filter(|t| t.meta.name == repo_name)
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
            .filter(|t| t.meta.name == repo_name)
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
                let is_multi = wizard.is_multi_repo;
                if description.is_empty() {
                    if is_multi {
                        // Multi-repo tasks require a description (the repo-inspector
                        // agent needs it to decide which repos are involved)
                        let wizard = self.wizard.as_mut().unwrap();
                        wizard.error_message =
                            Some("Multi-repo tasks require a description".to_string());
                        return Ok(());
                    }
                    // Empty description = setup-only mode
                    return self.create_setup_only_task_from_wizard();
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
            WizardStep::SelectBranch => {
                // Go back to directory picker for repo selection
                self.wizard = None;
                // Re-launch the directory picker
                if let Err(e) = self.start_wizard() {
                    self.set_status(format!("Error: {}", e));
                    self.view = View::TaskList;
                }
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

        let name = wizard.selected_repo_name().to_string();
        let repo_path = wizard.selected_repo_path.clone();
        let is_multi = wizard.is_multi_repo;

        let (branch_name, worktree_source) = match wizard.branch_source {
            BranchSource::ExistingWorktree => {
                let (branch, path) =
                    wizard.existing_worktrees[wizard.selected_worktree_index].clone();
                (branch, use_cases::WorktreeSource::ExistingWorktree(path))
            }
            BranchSource::NewBranch => {
                let bname = wizard.new_branch_editor.lines().join("").trim().to_string();
                let base = wizard.base_branch_editor.lines().join("").trim().to_string();
                let base_branch = if base.is_empty() { None } else { Some(base) };
                (bname, use_cases::WorktreeSource::NewBranch { base_branch })
            }
            BranchSource::ExistingBranch => {
                let bname = wizard.existing_branches[wizard.selected_branch_index].clone();
                (bname, use_cases::WorktreeSource::ExistingBranch)
            }
        };

        let description = wizard.description_editor.lines_joined().trim().to_string();
        let review_after = wizard.review_after;

        tracing::info!(name = %name, branch = %branch_name, is_multi, "creating task via wizard");
        self.log_output(format!("Creating task {}--{}...", name, branch_name));

        if is_multi {
            // Multi-repo path: use the path directly from the directory picker
            let parent_dir = repo_path;
            let task = match use_cases::create_multi_repo_task(
                &self.config,
                &name,
                &branch_name,
                &description,
                "new-multi",
                parent_dir.clone(),
                review_after,
            ) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to create multi-repo task");
                    self.log_output(format!("  Error: {}", e));
                    if let Some(w) = &mut self.wizard {
                        w.error_message = Some(format!("Failed to create task: {}", e));
                    }
                    return Ok(());
                }
            };

            // Create a temporary tmux session for the repo-inspector to run in
            // (working dir = parent directory)
            let tmux_session = Config::tmux_session_name(&name, &branch_name);
            self.log_output("  Creating tmux session for repo-inspector...".to_string());
            if let Err(e) = Tmux::create_session_with_windows(&tmux_session, &parent_dir) {
                tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to create tmux session for multi-repo task");
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to create tmux session: {}", e));
                }
                return Ok(());
            }

            let task_id = task.meta.task_id();
            let flow_cmd = format!("agman flow-run {}", task_id);
            let _ = Tmux::send_keys_to_window(&tmux_session, "agman", &flow_cmd);

            // Success - close wizard and refresh
            self.wizard = None;
            self.view = View::TaskList;
            self.refresh_tasks_and_select(&task_id);
            self.set_status(format!("Created multi-repo task: {}", task_id));
        } else {
            // Single-repo path: existing behavior
            let task = match use_cases::create_task(
                &self.config,
                &name,
                &branch_name,
                &description,
                "new",
                worktree_source,
                review_after,
            ) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to create task");
                    self.log_output(format!("  Error: {}", e));
                    if let Some(w) = &mut self.wizard {
                        w.error_message = Some(format!("Failed to create task: {}", e));
                    }
                    return Ok(());
                }
            };

            // Side effects: create tmux session and start flow
            let worktree_path = task.meta.primary_repo().worktree_path.clone();
            self.log_output("  Creating tmux session...".to_string());
            if let Err(e) = Tmux::create_session_with_windows(&task.meta.primary_repo().tmux_session, &worktree_path) {
                tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to create tmux session");
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to create tmux session: {}", e));
                }
                return Ok(());
            }

            let _ = Tmux::add_review_window(&task.meta.primary_repo().tmux_session, &worktree_path);

            let task_id = task.meta.task_id();
            let flow_cmd = format!("agman flow-run {}", task_id);
            let _ = Tmux::send_keys_to_window(&task.meta.primary_repo().tmux_session, "agman", &flow_cmd);

            // Success - close wizard and refresh
            self.wizard = None;
            self.view = View::TaskList;
            self.refresh_tasks_and_select(&task_id);
            self.set_status(format!("Created task: {}", task_id));
        }

        Ok(())
    }

    fn create_setup_only_task_from_wizard(&mut self) -> Result<()> {
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
                let base = wizard.base_branch_editor.lines().join("").trim().to_string();
                let base_branch = if base.is_empty() { None } else { Some(base) };
                (name, use_cases::WorktreeSource::NewBranch { base_branch })
            }
            BranchSource::ExistingBranch => {
                let name = wizard.existing_branches[wizard.selected_branch_index].clone();
                (name, use_cases::WorktreeSource::ExistingBranch)
            }
        };

        tracing::info!(repo = %repo_name, branch = %branch_name, "creating setup-only task via wizard");
        self.log_output(format!("Creating setup-only task {}--{}...", repo_name, branch_name));

        // Delegate business logic to use_cases
        let task = match use_cases::create_setup_only_task(
            &self.config,
            &repo_name,
            &branch_name,
            worktree_source,
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

        // Side effects: create tmux session (but do NOT start any flow)
        let worktree_path = task.meta.primary_repo().worktree_path.clone();
        self.log_output("  Creating tmux session...".to_string());
        if let Err(e) = Tmux::create_session_with_windows(&task.meta.primary_repo().tmux_session, &worktree_path) {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        let _ = Tmux::add_review_window(&task.meta.primary_repo().tmux_session, &worktree_path);

        // No flow-run command sent — this is the key difference from create_task_from_wizard

        // Success - close wizard and refresh
        let task_id = task.meta.task_id();
        self.wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id);
        self.set_status(format!("Created setup-only task: {}", task_id));

        Ok(())
    }

    // === Review Wizard Methods ===

    fn start_review_wizard(&mut self) -> Result<()> {
        if !self.config.repos_dir.exists() {
            // No repos_dir configured — use fallback picker to set repos_dir first
            let start = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            self.dir_picker = Some(DirectoryPicker::new(start, DirPickerOrigin::Review));
            self.view = View::DirectoryPicker;
            self.set_status(format!("No repos found in {}. Pick a repos directory (s to select, h/l to navigate).", self.config.repos_dir.display()));
            return Ok(());
        }

        let start = self.config.repos_dir.clone();
        self.dir_picker = Some(DirectoryPicker::new_with_favorites(
            start,
            DirPickerOrigin::ReviewRepoSelect,
            &self.config.repo_stats_path(),
            self.config.repos_dir.clone(),
        ));
        self.view = View::DirectoryPicker;
        Ok(())
    }

    fn review_wizard_next_step(&mut self) -> Result<()> {
        let wizard = match &mut self.review_wizard {
            Some(w) => w,
            None => return Ok(()),
        };

        wizard.error_message = None;

        match wizard.step {
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
    }

    fn review_wizard_prev_step(&mut self) {
        // Go back from EnterBranch → relaunch the DirectoryPicker in ReviewRepoSelect mode
        self.review_wizard = None;
        let _ = self.start_review_wizard();
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
        if let Some(pr_number) = lookup_pr_for_branch(&task.meta.primary_repo().worktree_path, &branch_name) {
            let task_id = task.meta.task_id();
            let wt = task.meta.primary_repo().worktree_path.clone();
            let author = fetch_pr_author(&wt, pr_number);
            match use_cases::set_linked_pr(&mut task, pr_number, &wt, false, author) {
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
        let worktree_path = task.meta.primary_repo().worktree_path.clone();
        self.log_output("  Creating tmux session...".to_string());
        if let Err(e) =
            Tmux::create_session_with_windows(&task.meta.primary_repo().tmux_session, &worktree_path)
        {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.review_wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        Tmux::add_review_window(&task.meta.primary_repo().tmux_session, &worktree_path)?;

        let task_id = task.meta.task_id();
        let review_cmd = format!("agman run-command {} review-pr", task_id);
        let _ = Tmux::send_keys_to_window(&task.meta.primary_repo().tmux_session, "agman", &review_cmd);

        // Success - close wizard and refresh
        self.review_wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id);
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
            View::SessionPicker => self.handle_session_picker_event(event),
            View::Notifications => self.handle_notifications_event(event),
            View::Notes => self.handle_notes_event(event),
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
                    self.refresh_tasks();
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
                            self.open_task_editor();
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
                KeyCode::Char('N') => {
                    self.selected_notif_index = 0;
                    self.view = View::Notifications;
                }
                KeyCode::Char('m') => {
                    match NotesView::new(self.config.notes_dir.clone()) {
                        Ok(nv) => {
                            self.notes_view = Some(nv);
                            self.view = View::Notes;
                        }
                        Err(e) => {
                            self.set_status(format!("Failed to open notes: {e}"));
                        }
                    }
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
                                if task.meta.is_multi_repo() && task.meta.repos.len() > 1 {
                                    // Multi-repo with multiple repos — show session picker
                                    let sessions: Vec<(String, String)> = task
                                        .meta
                                        .repos
                                        .iter()
                                        .filter(|r| Tmux::session_exists(&r.tmux_session))
                                        .map(|r| (r.repo_name.clone(), r.tmux_session.clone()))
                                        .collect();
                                    if !sessions.is_empty() {
                                        self.session_picker_sessions = sessions;
                                        self.selected_session_index = 0;
                                        self.view = View::SessionPicker;
                                    }
                                } else if task.meta.has_repos() {
                                    if Tmux::session_exists(&task.meta.primary_repo().tmux_session) {
                                        return Ok(true);
                                    }
                                } else if task.meta.is_multi_repo() {
                                    // Multi-repo task with no repos yet — try the parent session
                                    let parent_session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
                                    if Tmux::session_exists(&parent_session) {
                                        return Ok(true);
                                    }
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
                            self.open_task_editor();
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

                // If the task is in InputNeeded state, resume the flow after saving
                if self.selected_task().map_or(false, |t| t.meta.status == TaskStatus::InputNeeded) {
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
                        self.set_status("Restart available — press q to quit and restart".to_string());
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                    self.set_status("Restart available — press q to quit and restart".to_string());
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
                WizardStep::SelectBranch => {
                    match key.code {
                        KeyCode::Esc => {
                            self.wizard_prev_step();
                        }
                        KeyCode::Tab => {
                            // Multi-repo: locked to NewBranch, no cycling
                            if !wizard.is_multi_repo {
                                // Cycle forward: NewBranch → ExistingBranch → ExistingWorktree → NewBranch
                                wizard.branch_source = match wizard.branch_source {
                                    BranchSource::NewBranch => BranchSource::ExistingBranch,
                                    BranchSource::ExistingBranch => BranchSource::ExistingWorktree,
                                    BranchSource::ExistingWorktree => BranchSource::NewBranch,
                                };
                            }
                        }
                        KeyCode::BackTab => {
                            if !wizard.is_multi_repo {
                                // Cycle backward
                                wizard.branch_source = match wizard.branch_source {
                                    BranchSource::NewBranch => BranchSource::ExistingWorktree,
                                    BranchSource::ExistingBranch => BranchSource::NewBranch,
                                    BranchSource::ExistingWorktree => BranchSource::ExistingBranch,
                                };
                            }
                        }
                        KeyCode::Enter => {
                            self.wizard_next_step()?;
                        }
                        _ => {
                            match wizard.branch_source {
                                BranchSource::NewBranch => {
                                    // Ctrl+B or Up/Down toggles focus between branch name and base branch
                                    if key.modifiers.contains(KeyModifiers::CONTROL)
                                        && key.code == KeyCode::Char('b')
                                    {
                                        wizard.base_branch_focus = !wizard.base_branch_focus;
                                    } else if key.code == KeyCode::Up
                                        || key.code == KeyCode::Down
                                    {
                                        wizard.base_branch_focus = !wizard.base_branch_focus;
                                    } else if wizard.base_branch_focus {
                                        let input = Input::from(event.clone());
                                        wizard.base_branch_editor.input(input);
                                    } else {
                                        let input = Input::from(event.clone());
                                        wizard.new_branch_editor.input(input);
                                    }
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

    fn handle_session_picker_event(&mut self, event: Event) -> Result<bool> {
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
                    if !self.session_picker_sessions.is_empty() {
                        self.selected_session_index =
                            (self.selected_session_index + 1) % self.session_picker_sessions.len();
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !self.session_picker_sessions.is_empty() {
                        self.selected_session_index = if self.selected_session_index == 0 {
                            self.session_picker_sessions.len() - 1
                        } else {
                            self.selected_session_index - 1
                        };
                    }
                }
                KeyCode::Enter => {
                    if let Some((_, session)) =
                        self.session_picker_sessions.get(self.selected_session_index)
                    {
                        self.attach_session_name = Some(session.clone());
                        self.view = View::Preview;
                        return Ok(true);
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_notifications_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.notifications.is_empty()
                        && self.selected_notif_index < self.notifications.len() - 1
                    {
                        self.selected_notif_index += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if self.selected_notif_index > 0 {
                        self.selected_notif_index -= 1;
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(notif) = self.notifications.get(self.selected_notif_index) {
                        let thread_id = notif.id.clone();
                        let updated_at = notif.updated_at.clone();
                        tracing::info!(thread_id = %thread_id, "dismissing github notification");

                        // Track dismissed ID so polls don't reintroduce it (persisted to disk)
                        self.dismissed_notifs.insert(thread_id.clone(), updated_at);
                        self.dismissed_notifs.save(&self.config.dismissed_notifications_path());
                        tracing::info!(thread_id = %thread_id, "persisted dismissed notification");

                        // Optimistic removal
                        self.notifications.remove(self.selected_notif_index);
                        if self.selected_notif_index >= self.notifications.len()
                            && !self.notifications.is_empty()
                        {
                            self.selected_notif_index = self.notifications.len() - 1;
                        }

                        // Fire-and-forget background dismiss
                        self.rt.spawn(async move {
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Err(e) = use_cases::dismiss_github_notification(&thread_id) {
                                    tracing::warn!(thread_id = %thread_id, error = %e, "failed to dismiss notification");
                                }
                            })
                            .await;
                        });

                        self.set_status("Notification dismissed".to_string());
                    }
                }
                KeyCode::Char('o') | KeyCode::Enter => {
                    if let Some(notif) = self.notifications.get_mut(self.selected_notif_index) {
                        let url = notif.browser_url.clone();
                        let thread_id = notif.id.clone();
                        tracing::info!(url = %url, thread_id = %thread_id, "opening notification in browser");
                        open_url(&url);

                        // Optimistic mark-as-read
                        if notif.unread {
                            notif.unread = false;
                            tracing::info!(thread_id = %thread_id, "marking notification as read");

                            // Fire-and-forget background PATCH
                            self.rt.spawn(async move {
                                let _ = tokio::task::spawn_blocking(move || {
                                    if let Err(e) =
                                        use_cases::mark_notification_read(&thread_id)
                                    {
                                        tracing::warn!(thread_id = %thread_id, error = %e, "failed to mark notification as read");
                                    }
                                })
                                .await;
                            });

                        }

                        self.set_status("Opening notification...".to_string());
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_notes_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            let nv = match &mut self.notes_view {
                Some(nv) => nv,
                None => return Ok(false),
            };

            // Handle confirm_delete modal
            if nv.confirm_delete {
                match key.code {
                    KeyCode::Char('y') => {
                        if let Some(entry) = nv.entries.get(nv.selected_index) {
                            let path = if entry.is_dir {
                                nv.current_dir.join(&entry.file_name)
                            } else {
                                nv.current_dir.join(&entry.file_name)
                            };
                            if let Err(e) = use_cases::delete_note(&path) {
                                self.set_status(format!("Delete failed: {e}"));
                            }
                        }
                        let nv = self.notes_view.as_mut().unwrap();
                        nv.confirm_delete = false;
                        let _ = nv.refresh();
                    }
                    _ => {
                        nv.confirm_delete = false;
                    }
                }
                return Ok(false);
            }

            // Handle create_input modal
            if nv.create_input.is_some() {
                match key.code {
                    KeyCode::Enter => {
                        let (ref input, is_dir) = nv.create_input.as_ref().unwrap();
                        let name = input.lines()[0].trim().to_string();
                        if !name.is_empty() {
                            let current_dir = nv.current_dir.clone();
                            if *is_dir {
                                if let Err(e) = use_cases::create_note_dir(&current_dir, &name) {
                                    self.set_status(format!("Create dir failed: {e}"));
                                }
                            } else {
                                match use_cases::create_note(&current_dir, &name) {
                                    Ok(path) => {
                                        let nv = self.notes_view.as_mut().unwrap();
                                        nv.create_input = None;
                                        let _ = nv.refresh();
                                        let _ = nv.open_file(&path);
                                        nv.focus = NotesFocus::Editor;
                                        return Ok(false);
                                    }
                                    Err(e) => {
                                        self.set_status(format!("Create note failed: {e}"));
                                    }
                                }
                            }
                        }
                        let nv = self.notes_view.as_mut().unwrap();
                        nv.create_input = None;
                        let _ = nv.refresh();
                    }
                    KeyCode::Esc => {
                        nv.create_input = None;
                    }
                    _ => {
                        let input_event: Input = key.into();
                        nv.create_input.as_mut().unwrap().0.input(input_event);
                    }
                }
                return Ok(false);
            }

            // Handle rename_input modal
            if nv.rename_input.is_some() {
                match key.code {
                    KeyCode::Enter => {
                        let new_name = nv.rename_input.as_ref().unwrap().lines()[0].trim().to_string();
                        if !new_name.is_empty() {
                            if let Some(entry) = nv.entries.get(nv.selected_index) {
                                let old_path = nv.current_dir.join(&entry.file_name);
                                if let Err(e) = use_cases::rename_note(&old_path, &new_name) {
                                    self.set_status(format!("Rename failed: {e}"));
                                }
                            }
                        }
                        let nv = self.notes_view.as_mut().unwrap();
                        nv.rename_input = None;
                        let _ = nv.refresh();
                    }
                    KeyCode::Esc => {
                        nv.rename_input = None;
                    }
                    _ => {
                        let input_event: Input = key.into();
                        nv.rename_input.as_mut().unwrap().input(input_event);
                    }
                }
                return Ok(false);
            }

            // Main key handling based on focus
            match nv.focus {
                NotesFocus::Explorer => {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !nv.entries.is_empty() && nv.selected_index < nv.entries.len() - 1 {
                                nv.selected_index += 1;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if nv.selected_index > 0 {
                                nv.selected_index -= 1;
                            }
                        }
                        KeyCode::Char('l') | KeyCode::Enter => {
                            if let Some(entry) = nv.entries.get(nv.selected_index).cloned() {
                                if entry.is_dir {
                                    let new_dir = nv.current_dir.join(&entry.file_name);
                                    nv.current_dir = new_dir;
                                    nv.selected_index = 0;
                                    let _ = nv.refresh();
                                } else {
                                    let path = nv.current_dir.join(&entry.file_name);
                                    let _ = nv.save_current();
                                    if let Err(e) = nv.open_file(&path) {
                                        self.set_status(format!("Open failed: {e}"));
                                    } else {
                                        let nv = self.notes_view.as_mut().unwrap();
                                        nv.focus = NotesFocus::Editor;
                                    }
                                }
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Backspace => {
                            if nv.current_dir != nv.root_dir {
                                if let Some(parent) = nv.current_dir.parent() {
                                    let child_name = nv.current_dir.file_name()
                                        .map(|n| n.to_string_lossy().to_string());
                                    nv.current_dir = parent.to_path_buf();
                                    let _ = nv.refresh();
                                    nv.selected_index = child_name
                                        .and_then(|name| nv.entries.iter().position(|e| e.file_name == name))
                                        .unwrap_or(0);
                                }
                            }
                        }
                        KeyCode::Char('a') => {
                            nv.create_input = Some((TextArea::default(), false));
                        }
                        KeyCode::Char('A') => {
                            nv.create_input = Some((TextArea::default(), true));
                        }
                        KeyCode::Char('d') => {
                            if !nv.entries.is_empty() {
                                nv.confirm_delete = true;
                            }
                        }
                        KeyCode::Char('r') => {
                            if let Some(entry) = nv.entries.get(nv.selected_index) {
                                let mut ta = TextArea::default();
                                ta.insert_str(&entry.name);
                                nv.rename_input = Some(ta);
                            }
                        }
                        KeyCode::Tab => {
                            if nv.open_file.is_some() {
                                nv.focus = NotesFocus::Editor;
                            }
                        }
                        KeyCode::Char('J') => {
                            if !nv.entries.is_empty() && nv.selected_index < nv.entries.len() - 1 {
                                let entry_name = nv.entries[nv.selected_index].file_name.clone();
                                let dir = nv.current_dir.clone();
                                match use_cases::move_note(&dir, &entry_name, use_cases::MoveDirection::Down) {
                                    Ok(new_idx) => {
                                        let _ = nv.refresh();
                                        nv.selected_index = new_idx;
                                    }
                                    Err(e) => {
                                        self.set_status(format!("Move failed: {e}"));
                                    }
                                }
                            }
                        }
                        KeyCode::Char('K') => {
                            if nv.selected_index > 0 {
                                let entry_name = nv.entries[nv.selected_index].file_name.clone();
                                let dir = nv.current_dir.clone();
                                match use_cases::move_note(&dir, &entry_name, use_cases::MoveDirection::Up) {
                                    Ok(new_idx) => {
                                        let _ = nv.refresh();
                                        nv.selected_index = new_idx;
                                    }
                                    Err(e) => {
                                        self.set_status(format!("Move failed: {e}"));
                                    }
                                }
                            }
                        }
                        KeyCode::Char('x') => {
                            if let Some(entry) = nv.entries.get(nv.selected_index) {
                                let cut = (nv.current_dir.clone(), entry.file_name.clone());
                                let status_msg = format!("Cut: {}", entry.name);
                                tracing::info!(dir = %cut.0.display(), file = %cut.1, "cut note");
                                nv.cut_entry = Some(cut);
                                self.set_status(status_msg);
                            }
                        }
                        KeyCode::Char('p') => {
                            if let Some((ref src_dir, ref file_name)) = nv.cut_entry.clone() {
                                let dest_dir = nv.current_dir.clone();
                                match use_cases::paste_note(src_dir, &dest_dir, file_name) {
                                    Ok(()) => {
                                        let nv = self.notes_view.as_mut().unwrap();
                                        nv.cut_entry = None;
                                        let _ = nv.refresh();
                                        self.set_status(format!("Pasted: {}", file_name));
                                    }
                                    Err(e) => {
                                        self.set_status(format!("Paste failed: {e}"));
                                    }
                                }
                            }
                        }
                        KeyCode::Esc => {
                            if nv.cut_entry.is_some() {
                                nv.cut_entry = None;
                                self.set_status("Cut cancelled".to_string());
                            } else {
                                let _ = nv.save_current();
                                self.notes_view = None;
                                self.view = View::TaskList;
                            }
                        }
                        KeyCode::Char('q') => {
                            let _ = nv.save_current();
                            self.notes_view = None;
                            self.view = View::TaskList;
                        }
                        _ => {}
                    }
                }
                NotesFocus::Editor => {
                    let vim_mode = nv.editor.mode();
                    let is_normal = vim_mode == VimMode::Normal;

                    if key.code == KeyCode::Tab && is_normal {
                        let _ = nv.save_current();
                        nv.focus = NotesFocus::Explorer;
                    } else if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                        let _ = nv.save_current();
                        self.set_status("Saved".to_string());
                    } else if key.code == KeyCode::Char('q') && is_normal {
                        let _ = nv.save_current();
                        self.notes_view = None;
                        self.view = View::TaskList;
                    } else {
                        let before = nv.editor.lines_joined();
                        nv.editor.input(key.into());
                        let after = nv.editor.lines_joined();
                        if before != after {
                            nv.modified = true;
                        }
                    }
                }
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
                Some(t) => {
                    let tmux_session = if t.meta.has_repos() {
                        t.meta.primary_repo().tmux_session.clone()
                    } else if t.meta.is_multi_repo() {
                        Config::tmux_session_name(&t.meta.name, &t.meta.branch_name)
                    } else {
                        return Ok(());
                    };
                    (
                        t.meta.task_id(),
                        t.meta.status,
                        t.meta.flow_name.clone(),
                        tmux_session,
                        t.read_task().unwrap_or_else(|_| "No TASK.md available".to_string()),
                    )
                }
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
                                    .filter(|t| t.meta.has_repos())
                                    .map(|t| t.meta.primary_repo().worktree_path.clone());
                                if let Some(wt) = worktree_path {
                                    let author = if !owned {
                                        fetch_pr_author(&wt, pr_number)
                                    } else {
                                        None
                                    };
                                    if let Some(task) =
                                        self.tasks.get_mut(self.selected_index)
                                    {
                                        match use_cases::set_linked_pr(task, pr_number, &wt, owned, author.clone()) {
                                            Ok(()) => {
                                                let label = if owned { "mine".to_string() } else {
                                                    author.unwrap_or_else(|| "ext".to_string())
                                                };
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
                    self.dir_picker = None;
                    self.view = View::TaskList;
                    self.set_status("Directory selection cancelled".to_string());
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(picker) = &mut self.dir_picker {
                        let total = picker.total_items();
                        if total > 0 {
                            picker.selected_index =
                                (picker.selected_index + 1) % total;
                        }
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(picker) = &mut self.dir_picker {
                        let total = picker.total_items();
                        if total > 0 {
                            picker.selected_index = if picker.selected_index == 0 {
                                total - 1
                            } else {
                                picker.selected_index - 1
                            };
                        }
                    }
                }
                KeyCode::Char('l') | KeyCode::Enter => {
                    // In RepoSelect/ReviewRepoSelect mode: Enter on a git repo or favourite selects it directly
                    let should_select = self.dir_picker.as_ref().map(|p| {
                        p.is_repo_select_mode()
                            && (p.is_favorite_selected() || p.selected_entry_kind() == Some(DirKind::GitRepo))
                    }).unwrap_or(false);

                    if should_select {
                        self.select_repo_from_picker()?;
                    } else if let Some(picker) = &mut self.dir_picker {
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
                    let origin = self.dir_picker.as_ref().map(|p| p.origin);
                    match origin {
                        Some(DirPickerOrigin::RepoSelect) => {
                            // Select the highlighted entry as a repo or multi-repo parent
                            self.select_repo_from_picker()?;
                        }
                        Some(DirPickerOrigin::ReviewRepoSelect) => {
                            // For review: favourites and git repos can be selected, multi-repo navigates in
                            let is_fav = self.dir_picker.as_ref().map(|p| p.is_favorite_selected()).unwrap_or(false);
                            let kind = self.dir_picker.as_ref().and_then(|p| p.selected_entry_kind());
                            if is_fav || kind == Some(DirKind::GitRepo) {
                                self.select_repo_from_picker()?;
                            } else if let Some(picker) = &mut self.dir_picker {
                                // MultiRepoParent or Plain: navigate into instead of selecting
                                picker.enter_selected();
                            }
                        }
                        Some(DirPickerOrigin::NewTask) | Some(DirPickerOrigin::Review) => {
                            // Select current directory as repos_dir (fallback mode)
                            if let Some(picker) = self.dir_picker.take() {
                                let selected_dir = picker.current_dir.clone();
                                let origin = picker.origin;

                                let config_file = agman::config::ConfigFile {
                                    repos_dir: Some(selected_dir.to_string_lossy().to_string()),
                                };
                                if let Err(e) = agman::config::save_config_file(&self.config.base_dir, &config_file) {
                                    self.set_status(format!("Failed to save config: {}", e));
                                    self.view = View::TaskList;
                                    return Ok(false);
                                }

                                self.config.repos_dir = selected_dir;
                                tracing::info!(repos_dir = %self.config.repos_dir.display(), "repos_dir updated via directory picker");

                                match origin {
                                    DirPickerOrigin::NewTask => {
                                        self.start_wizard()?;
                                    }
                                    DirPickerOrigin::Review => {
                                        self.start_review_wizard()?;
                                    }
                                    DirPickerOrigin::RepoSelect | DirPickerOrigin::ReviewRepoSelect => unreachable!(),
                                }
                            }
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Handle a repo selection from the directory picker (RepoSelect/ReviewRepoSelect mode).
    fn select_repo_from_picker(&mut self) -> Result<()> {
        let (entry_kind, entry_path, entry_name, origin, is_fav) = match &self.dir_picker {
            Some(picker) => {
                let kind = picker.selected_entry_kind().unwrap_or(DirKind::Plain);
                let path = match picker.selected_path() {
                    Some(p) => p,
                    None => return Ok(()),
                };
                let name = picker.selected_name().unwrap_or_default();
                let is_fav = picker.is_favorite_selected();
                (kind, path, name, picker.origin, is_fav)
            }
            None => return Ok(()),
        };

        if is_fav {
            tracing::info!(repo = %entry_name, "selected favourite repo from picker");
        }

        match origin {
            DirPickerOrigin::RepoSelect => match entry_kind {
                DirKind::GitRepo => {
                    self.dir_picker = None;
                    tracing::info!(repo = %entry_name, path = %entry_path.display(), "selected git repo from picker");
                    self.create_wizard_from_picker(entry_name, entry_path, false)?;
                }
                DirKind::MultiRepoParent => {
                    self.dir_picker = None;
                    tracing::info!(parent = %entry_name, path = %entry_path.display(), "selected multi-repo parent from picker");
                    self.create_wizard_from_picker(entry_name, entry_path, true)?;
                }
                DirKind::Plain => {
                    if let Some(picker) = &mut self.dir_picker {
                        picker.enter_selected();
                    }
                }
            },
            DirPickerOrigin::ReviewRepoSelect => match entry_kind {
                DirKind::GitRepo => {
                    self.dir_picker = None;
                    tracing::info!(repo = %entry_name, path = %entry_path.display(), "selected git repo from review picker");
                    self.create_review_wizard_from_picker(entry_name)?;
                }
                DirKind::MultiRepoParent | DirKind::Plain => {
                    // Navigate into — review wizard only accepts single git repos
                    if let Some(picker) = &mut self.dir_picker {
                        picker.enter_selected();
                    }
                }
            },
            _ => {
                // Fallback origins shouldn't reach here
                if let Some(picker) = &mut self.dir_picker {
                    picker.enter_selected();
                }
            }
        }
        Ok(())
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
            .filter(|t| {
                t.meta.status == TaskStatus::Stopped
                    && t.meta.has_repos()
                    && t.meta.linked_pr.as_ref().is_some_and(|pr| pr.owned)
            })
            .map(|t| {
                let pr = t.meta.linked_pr.as_ref().unwrap();
                (
                    t.meta.task_id(),
                    pr.number,
                    t.meta.primary_repo().worktree_path.clone(),
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

                // Kill all tmux sessions for the task
                for repo in &task.meta.repos {
                    let _ = Tmux::kill_session(&repo.tmux_session);
                }
                if task.meta.is_multi_repo() {
                    let parent_session = Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
                    let _ = Tmux::kill_session(&parent_session);
                }

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

    /// Spawn a background task to poll GitHub notifications.
    fn start_gh_notif_poll(&mut self) {
        if self.gh_notif_poll_active {
            return;
        }

        self.gh_notif_poll_active = true;
        let tx = self.gh_notif_tx.clone();

        tracing::debug!("starting github notification poll");
        self.rt.spawn(async move {
            let result = tokio::task::spawn_blocking(use_cases::fetch_github_notifications)
                .await
                .unwrap_or_else(|_| use_cases::NotifPollResult {
                    notifications: Vec::new(),
                });
            let _ = tx.send(result);
        });
    }

    /// Check for completed notification poll results (non-blocking) and apply.
    fn apply_gh_notif_results(&mut self) {
        let result = match self.gh_notif_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        self.gh_notif_poll_active = false;

        if !self.gh_notif_first_poll_done {
            self.gh_notif_first_poll_done = true;
            tracing::debug!("first github notification poll completed");
        }

        self.notifications = result.notifications;

        // Filter out notifications that were dismissed but may not yet be reflected by the API.
        // Remove IDs from the dismissed set only when they no longer appear in poll results
        // (confirming the server processed the DELETE). Keep IDs that are still present
        // (the DELETE may still be in-flight).
        if !self.dismissed_notifs.ids.is_empty() {
            let fetched_ids: HashSet<&str> = self.notifications.iter().map(|n| n.id.as_str()).collect();
            // IDs not in the fetched results are confirmed deleted — remove from tracking set
            let before_cleanup = self.dismissed_notifs.ids.len();
            self.dismissed_notifs.ids.retain(|id, _entry| fetched_ids.contains(id.as_str()));
            // Prune entries older than the retention window
            let retention = chrono::Duration::weeks(agman::dismissed_notifications::NOTIFICATION_RETENTION_WEEKS);
            self.dismissed_notifs.prune_older_than(retention);

            // Un-dismiss threads that have new activity since they were dismissed
            let mut undismissed: Vec<(String, String, String)> = Vec::new();
            for notif in &self.notifications {
                if self.dismissed_notifs.should_undismiss(&notif.id, &notif.updated_at) {
                    let old_updated_at = self.dismissed_notifs.ids.get(&notif.id)
                        .map(|e| e.updated_at.clone())
                        .unwrap_or_default();
                    undismissed.push((notif.id.clone(), old_updated_at, notif.updated_at.clone()));
                }
            }
            for (thread_id, old_updated_at, new_updated_at) in &undismissed {
                tracing::info!(
                    thread_id = %thread_id,
                    old_updated_at = %old_updated_at,
                    new_updated_at = %new_updated_at,
                    "un-dismissing notification due to new activity"
                );
                self.dismissed_notifs.remove(thread_id);
            }

            let changed = self.dismissed_notifs.ids.len() < before_cleanup || !undismissed.is_empty();
            if changed {
                self.dismissed_notifs.save(&self.config.dismissed_notifications_path());
                tracing::debug!(
                    removed = before_cleanup.saturating_sub(self.dismissed_notifs.ids.len()),
                    undismissed = undismissed.len(),
                    "cleaned up dismissed notification entries"
                );
            }
            // Filter out still-dismissed notifications from the displayed list
            let before = self.notifications.len();
            self.notifications.retain(|n| !self.dismissed_notifs.contains(&n.id));
            let filtered = before - self.notifications.len();
            if filtered > 0 {
                tracing::debug!(filtered_count = filtered, "filtered dismissed notifications from poll results");
            }
        }

        // Clamp selection index
        if self.selected_notif_index >= self.notifications.len() && !self.notifications.is_empty() {
            self.selected_notif_index = self.notifications.len() - 1;
        }

        tracing::debug!(notification_count = self.notifications.len(), "applied github notification poll results");
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

        let task_meta = &self.tasks[task_idx].meta;
        let (tmux_session, working_dir) = if task_meta.has_repos() {
            (
                task_meta.primary_repo().tmux_session.clone(),
                task_meta.primary_repo().worktree_path.clone(),
            )
        } else if task_meta.is_multi_repo() {
            (
                Config::tmux_session_name(&task_meta.name, &task_meta.branch_name),
                task_meta.parent_dir.clone().unwrap_or_default(),
            )
        } else {
            self.set_status(format!("Task {} has no repos configured", task_id));
            self.restart_wizard = None;
            self.view = View::TaskList;
            return Ok(());
        };

        // Set flow_step and status
        use_cases::restart_task(&mut self.tasks[task_idx], selected_step_index)?;
        // Clear review_addressed on restart
        let _ = use_cases::set_review_addressed(&mut self.tasks[task_idx], false);

        // Ensure tmux session exists
        if !Tmux::session_exists(&tmux_session) {
            let _ = Tmux::create_session_with_windows(&tmux_session, &working_dir);
            if self.tasks[task_idx].meta.has_repos() {
                let _ = Tmux::add_review_window(&tmux_session, &working_dir);
            }
        }

        // Dispatch flow-run
        let flow_cmd = format!("agman flow-run {}", task_id);
        let _ = Tmux::send_keys_to_window(&tmux_session, "agman", &flow_cmd);

        // Clean up wizard
        self.restart_wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id);
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
        app.refresh_tasks();

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
                    // Session picker sets attach_session_name directly
                    if let Some(session) = app.attach_session_name.take() {
                        attach_session = Some(session);
                    } else if let Some(task) = app.selected_task() {
                        if task.meta.has_repos() {
                            attach_session = Some(task.meta.primary_repo().tmux_session.clone());
                        } else if task.meta.is_multi_repo() {
                            // Multi-repo with no repos yet — attach to parent session
                            attach_session = Some(Config::tmux_session_name(&task.meta.name, &task.meta.branch_name));
                        }
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
                    app.refresh_tasks();
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

            // Poll GitHub notifications every 60 seconds (regardless of view)
            if app.last_gh_notif_poll.elapsed() >= Duration::from_secs(60) {
                app.start_gh_notif_poll();
                app.last_gh_notif_poll = Instant::now();
            }

            // Check for completed notification poll results (non-blocking)
            app.apply_gh_notif_results();

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
