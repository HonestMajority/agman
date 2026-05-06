use anyhow::Result;
use ratatui::crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, widgets::ListState, Terminal};
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tui_textarea::{CursorMove, Input, Key, TextArea};

use agman::command::StoredCommand;
use agman::config::Config;
use agman::dismissed_notifications::DismissedNotifications;
use agman::flow::Flow;
use agman::git::Git;
use agman::inbox;
use agman::project::Project;
use agman::repo_stats::RepoStats;
use agman::researcher::Researcher;
use agman::supervisor;
use agman::task::{Task, TaskStatus};
use agman::tmux::Tmux;
use agman::use_cases;

use super::ui;
use super::vim::{VimMode, VimTextArea};

/// Telegram watchdog: how often the main loop checks the bot heartbeat.
const TELEGRAM_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// Telegram watchdog: heartbeat age past which the bot thread is considered
/// stalled and gets respawned. The bot writes its heartbeat once per poll
/// cycle (~1s), so 60s means ~60 missed cycles.
const TELEGRAM_STALL_THRESHOLD_SECS: u64 = 60;
/// Telegram watchdog: cooldown after a respawn before another can fire.
/// Gives the new thread time to warm up and write its first heartbeat.
const TELEGRAM_RESPAWN_COOLDOWN: Duration = Duration::from_secs(90);

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
    ProjectList,
    TaskList,
    Preview,
    DeleteConfirm,
    Feedback,
    NewTaskWizard,
    CommandList,
    TaskEditor,
    Queue,
    RebaseBranchPicker,
    ReviewWizard,
    RestartWizard,
    DirectoryPicker,
    SessionPicker,
    Notifications,
    Notes,
    ShowPrs,
    Settings,
    Archive,
    ProjectWizard,
    ProjectPicker,
    ProjectDeleteConfirm,
    ResearcherList,
    ResearcherWizard,
    RespawnConfirm,
}

/// A live tmux popup spawned by agman. The `Child` is polled each tick of
/// the main loop via `try_wait` so the loop never blocks on the popup.
struct ActivePopup {
    child: std::process::Child,
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
        matches!(
            self.origin,
            DirPickerOrigin::RepoSelect | DirPickerOrigin::ReviewRepoSelect
        )
    }

    fn refresh_entries(&mut self) {
        self.entries.clear();
        self.entry_kinds.clear();
        if let Ok(read_dir) = std::fs::read_dir(&self.current_dir) {
            let is_repo_select = self.is_repo_select_mode();
            let mut dirs: Vec<(String, PathBuf)> = read_dir
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
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
            return Some(
                self.repos_dir
                    .join(&self.favorite_repos[self.selected_index].0),
            );
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
    /// The actual filesystem path of the repo (may be outside repos_dir).
    pub selected_repo_path: PathBuf,
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

/// What triggered the project picker modal.
#[derive(Debug, Clone)]
pub enum ProjectPickerAction {
    /// Migrate all unassigned tasks to the selected project.
    MigrateAllUnassigned,
}

pub struct ProjectPicker {
    pub projects: Vec<String>,
    pub selected: usize,
    pub action: ProjectPickerAction,
}

pub struct ProjectWizard {
    pub name_editor: TextArea<'static>,
    pub description_editor: VimTextArea<'static>,
    /// false = name field focused, true = description field focused
    pub description_focus: bool,
    pub error_message: Option<String>,
}

pub struct ResearcherWizard {
    pub name_editor: TextArea<'static>,
    pub description_editor: VimTextArea<'static>,
    /// false = name field focused, true = description field focused
    pub description_focus: bool,
    pub error_message: Option<String>,
    pub project: String,
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

struct InboxPollResult {
    target: String, // "chief-of-staff" or project name
    delivered: usize,
    errors: Vec<String>,
}

struct InboxPollOutput {
    results: Vec<InboxPollResult>,
    stuck_skip_counts: std::collections::HashMap<String, u32>,
    first_ready_at: std::collections::HashMap<String, Instant>,
}

/// One supervisor tick result for a single Running task.
///
/// Collected in the background by `start_supervisor_poll` and drained in the
/// main loop by `apply_supervisor_poll_results`. `Tick` carries a `PollOutcome`
/// from a live session; `NeedsLaunch` signals a half-state task (Running with
/// a stopped last session) that should re-enter `launch_next_step`.
#[derive(Debug)]
enum SupervisorPollItem {
    Tick {
        task_id: String,
        session_name: String,
        outcome: supervisor::PollOutcome,
    },
    NeedsLaunch {
        task_id: String,
    },
}

#[derive(Debug)]
struct SupervisorPollOutput {
    items: Vec<SupervisorPollItem>,
}

/// How many consecutive "skipped" poll cycles (readiness gate refused to
/// deliver while the inbox still had undelivered messages) qualifies a target
/// as "stalled" and surfaces a UI indicator. At the 2s poll cadence this is
/// ~10 seconds.
pub const STALL_THRESHOLD: u32 = 5;

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
            "pr",
            "list",
            "--head",
            branch_name,
            "--json",
            "number",
            "--limit",
            "1",
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
        .args(["pr", "view", &pr_number.to_string(), "--json", "author"])
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
    pub logs_editor: VimTextArea<'static>,
    pub notes_content: String,
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
    pub rebase_branch_list_state: ListState,
    pub pending_branch_command: Option<StoredCommand>,
    pub rebase_branch_search: TextArea<'static>,
    // Delete mode chooser
    pub archive_mode_index: usize,
    // Restart task wizard
    pub restart_wizard: Option<RestartWizard>,
    pub should_restart: bool,
    // PR polling
    pub last_pr_poll: Instant,
    pr_poll_tx: tokio_mpsc::UnboundedSender<Vec<PrPollResult>>,
    pr_poll_rx: tokio_mpsc::UnboundedReceiver<Vec<PrPollResult>>,
    pr_poll_active: bool,
    // Tokio runtime for background async work
    rt: tokio::runtime::Runtime,
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
    // Keybase unread polling
    pub keybase_dm_unread_count: usize,
    pub keybase_channel_unread_count: usize,
    pub last_keybase_poll: Instant,
    keybase_tx: tokio_mpsc::UnboundedSender<use_cases::KeybasePollResult>,
    keybase_rx: tokio_mpsc::UnboundedReceiver<use_cases::KeybasePollResult>,
    keybase_poll_active: bool,
    pub keybase_first_poll_done: bool,
    pub keybase_available: bool,
    // Notes view
    pub notes_view: Option<NotesView>,
    // Show PRs (GitHub Issues & PRs for current user)
    pub show_prs_data: use_cases::ShowPrsData,
    pub show_prs_selected: usize,
    pub show_prs_first_poll_done: bool,
    pub show_prs_list_state: ListState,
    show_prs_poll_tx: tokio_mpsc::UnboundedSender<use_cases::ShowPrsData>,
    show_prs_poll_rx: tokio_mpsc::UnboundedReceiver<use_cases::ShowPrsData>,
    show_prs_poll_active: bool,
    pub last_show_prs_poll: Instant,
    // Settings view
    pub settings_selected: usize,
    pub settings_editing: bool,
    pub archive_retention_days: u64,
    pub telegram_token_editor: TextArea<'static>,
    pub telegram_chat_id_editor: TextArea<'static>,
    // Archive view
    pub archive_tasks: Vec<(Task, String)>,
    pub archive_search: TextArea<'static>,
    pub archive_selected: usize,
    pub archive_list_state: ListState,
    pub archive_preview: Option<String>,
    pub archive_scroll: u16,
    // Project list (Chief of Staff/PM hierarchy)
    pub projects: Vec<Project>,
    pub selected_project_index: usize,
    pub current_project: Option<String>,
    pub unassigned_task_count: usize,
    pub unassigned_unseen_stopped_count: usize,
    pub project_task_counts: std::collections::HashMap<String, (usize, usize, usize)>, // (total, active, unseen_stopped)
    // Project wizard
    pub project_wizard: Option<ProjectWizard>,
    pub researcher_wizard: Option<ResearcherWizard>,
    // Project picker (for task migration/move)
    pub project_picker: Option<ProjectPicker>,
    // Project deletion
    pub project_to_delete: Option<String>,
    // Researchers
    pub researchers: Vec<Researcher>,
    pub researcher_list_index: usize,
    // Inbox polling
    pub last_inbox_poll: Instant,
    inbox_poll_tx: tokio_mpsc::UnboundedSender<InboxPollOutput>,
    inbox_poll_rx: tokio_mpsc::UnboundedReceiver<InboxPollOutput>,
    inbox_poll_active: bool,
    // Supervisor polling (drives interactive-claude flow progression)
    pub last_supervisor_poll: Instant,
    supervisor_poll_tx: tokio_mpsc::UnboundedSender<SupervisorPollOutput>,
    supervisor_poll_rx: tokio_mpsc::UnboundedReceiver<SupervisorPollOutput>,
    supervisor_poll_active: bool,
    stuck_skip_counts: std::collections::HashMap<String, u32>,
    /// First time each target was observed in the "ready" state. Used as a
    /// 3-second cold-start buffer: deliveries are gated until this much time
    /// has elapsed since the readiness flip. Cleared when the target leaves
    /// the ready state (kill+relaunch re-arms the buffer).
    first_ready_at: std::collections::HashMap<String, Instant>,
    /// Targets we've already emitted a one-shot "stalled" warning for this
    /// episode — prevents a per-cycle warn spam. Cleared when the target
    /// recovers (falls below `STALL_THRESHOLD`).
    stall_warned: std::collections::HashSet<String>,
    // Agent respawn
    pub respawn_in_progress: Option<String>,
    respawn_tx: tokio_mpsc::UnboundedSender<Result<String, String>>,
    respawn_rx: tokio_mpsc::UnboundedReceiver<Result<String, String>>,
    // Respawn confirmation dialog
    pub respawn_confirm_target: Option<String>,
    pub respawn_confirm_index: usize,
    pub respawn_confirm_is_chief_of_staff: bool,
    pub respawn_confirm_return_view: View,
    // Telegram bot handle (None when not configured). Carries the cancel flag,
    // a heartbeat atomic the TUI reads to render a health indicator, and the
    // join handle (held only for potential future clean shutdown).
    pub telegram: Option<agman::telegram::TelegramHandle>,
    /// Throttle for the telegram-bot watchdog heartbeat check.
    last_telegram_watchdog: Instant,
    /// Most recent watchdog-driven respawn, used for the cooldown guard.
    last_telegram_respawn_at: Option<Instant>,
    // Sleep inhibition (macOS: caffeinate -s for system sleep assertion only)
    #[cfg(target_os = "macos")]
    caffeinate_process: Option<std::process::Child>,
    // Active tmux popup (CoS/PM chat, researcher attach). Polled each main-loop
    // tick so inbox delivery and PR polls keep running while a
    // popup is open.
    popup: Option<ActivePopup>,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        use_cases::migrate_old_tasks(&config);
        match use_cases::purge_old_archives(&config) {
            Ok(count) if count > 0 => {
                tracing::info!(purged = count, "purged expired archived tasks on startup");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "failed to purge old archives on startup");
            }
        }
        let tasks = Task::list_all(&config);
        let commands = StoredCommand::list_all(&config.commands_dir).unwrap_or_default();
        let notes_editor = VimTextArea::new();
        let mut logs_editor = VimTextArea::new();
        logs_editor.set_read_only(true);
        let feedback_editor = VimTextArea::new();
        let task_file_editor = VimTextArea::new();
        let (pr_poll_tx, pr_poll_rx) = tokio_mpsc::unbounded_channel();
        let (gh_notif_tx, gh_notif_rx) = tokio_mpsc::unbounded_channel();
        let (show_prs_poll_tx, show_prs_poll_rx) = tokio_mpsc::unbounded_channel();
        let (keybase_tx, keybase_rx) = tokio_mpsc::unbounded_channel();
        let (inbox_poll_tx, inbox_poll_rx) = tokio_mpsc::unbounded_channel();
        let (supervisor_poll_tx, supervisor_poll_rx) = tokio_mpsc::unbounded_channel();
        let (respawn_tx, respawn_rx) = tokio_mpsc::unbounded_channel();
        let rt = tokio::runtime::Runtime::new()?;
        let mut dismissed_notifs =
            DismissedNotifications::load(&config.dismissed_notifications_path());
        let retention =
            chrono::Duration::weeks(agman::dismissed_notifications::NOTIFICATION_RETENTION_WEEKS);
        if dismissed_notifs.prune_older_than(retention) > 0 {
            dismissed_notifs.save(&config.dismissed_notifications_path());
        }

        // Auto-start the Chief of Staff agent session in the background
        if let Err(e) = use_cases::start_chief_of_staff_session(&config, false) {
            tracing::error!(error = %e, "failed to auto-start Chief of Staff session on launch");
        }

        // Auto-start PM sessions for all projects
        if let Ok(projects) = use_cases::list_projects(&config) {
            for project in &projects {
                if let Err(e) = use_cases::start_pm_session(&config, &project.meta.name, false) {
                    tracing::error!(project = %project.meta.name, error = %e, "failed to auto-start PM session on launch");
                }
            }
        }

        // Start Telegram bot if settings are configured. `start` returns None
        // when token/chat_id are empty, so the match here just unwraps the
        // configured case.
        let (tg_token, tg_chat_id) = use_cases::load_telegram_config(&config);
        let telegram = match (&tg_token, &tg_chat_id) {
            (Some(token), Some(chat_id)) => {
                agman::telegram::start(&config, token.clone(), chat_id.clone())
            }
            _ => None,
        };

        let mut telegram_token_editor = Self::create_plain_editor();
        if let Some(ref token) = tg_token {
            telegram_token_editor = TextArea::new(vec![token.clone()]);
            telegram_token_editor.set_cursor_line_style(ratatui::style::Style::default());
        }
        let mut telegram_chat_id_editor = Self::create_plain_editor();
        if let Some(ref chat_id) = tg_chat_id {
            telegram_chat_id_editor = TextArea::new(vec![chat_id.clone()]);
            telegram_chat_id_editor.set_cursor_line_style(ratatui::style::Style::default());
        }

        let archive_retention_days = use_cases::load_archive_retention(&config);

        Ok(Self {
            config,
            tasks,
            selected_index: 0,
            view: View::ProjectList,
            preview_content: String::new(),
            logs_editor,
            notes_content: String::new(),
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
            rebase_branch_list_state: ListState::default(),
            pending_branch_command: None,
            rebase_branch_search: Self::create_plain_editor(),
            archive_mode_index: 0,
            restart_wizard: None,
            should_restart: false,
            last_pr_poll: Instant::now(),
            pr_poll_tx,
            pr_poll_rx,
            pr_poll_active: false,
            rt,
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
            keybase_dm_unread_count: 0,
            keybase_channel_unread_count: 0,
            last_keybase_poll: Instant::now() - Duration::from_secs(2),
            keybase_tx,
            keybase_rx,
            keybase_poll_active: false,
            keybase_first_poll_done: false,
            keybase_available: true,
            notes_view: None,
            show_prs_data: Default::default(),
            show_prs_selected: 0,
            show_prs_first_poll_done: false,
            show_prs_list_state: ListState::default(),
            show_prs_poll_tx,
            show_prs_poll_rx,
            show_prs_poll_active: false,
            last_show_prs_poll: Instant::now() - Duration::from_secs(60),
            settings_selected: 0,
            settings_editing: false,
            archive_retention_days,
            telegram_token_editor,
            telegram_chat_id_editor,
            archive_tasks: Vec::new(),
            archive_search: Self::create_plain_editor(),
            archive_selected: 0,
            archive_list_state: ListState::default(),
            archive_preview: None,
            archive_scroll: 0,
            projects: Vec::new(),
            selected_project_index: 0,
            current_project: None,
            unassigned_task_count: 0,
            unassigned_unseen_stopped_count: 0,
            project_task_counts: std::collections::HashMap::new(),
            project_wizard: None,
            researcher_wizard: None,
            project_picker: None,
            project_to_delete: None,
            researchers: Vec::new(),
            researcher_list_index: 0,
            last_inbox_poll: Instant::now(),
            inbox_poll_tx,
            inbox_poll_rx,
            inbox_poll_active: false,
            last_supervisor_poll: Instant::now(),
            supervisor_poll_tx,
            supervisor_poll_rx,
            supervisor_poll_active: false,
            stuck_skip_counts: std::collections::HashMap::new(),
            first_ready_at: std::collections::HashMap::new(),
            stall_warned: std::collections::HashSet::new(),
            respawn_in_progress: None,
            respawn_tx,
            respawn_rx,
            respawn_confirm_target: None,
            respawn_confirm_index: 0,
            respawn_confirm_is_chief_of_staff: false,
            respawn_confirm_return_view: View::ProjectList,
            telegram,
            last_telegram_watchdog: Instant::now(),
            last_telegram_respawn_at: None,
            #[cfg(target_os = "macos")]
            caffeinate_process: std::process::Command::new("caffeinate")
                .arg("-s")
                .spawn()
                .ok(),
            popup: None,
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

    /// Dispatch side-effects for a popup that just closed. Called from the
    /// main loop once `try_wait` on the popup `Child` returns `Some`.
    fn on_popup_closed(&mut self) {
        if self.popup.take().is_some() {
            tracing::info!("popup closed");
        }
    }

    fn create_plain_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(ratatui::style::Style::default());
        editor
    }

    /// Check the Telegram bot heartbeat and respawn the thread if it has
    /// stalled. The bot bumps its `heartbeat` atomic (epoch seconds) once
    /// per poll cycle; a stale value means the thread is parked in a
    /// syscall `catch_unwind` can't recover from (e.g. macOS `getaddrinfo`
    /// with no timeout) and only a respawn will unblock the bridge.
    ///
    /// The stuck thread leaks — `cancel` is set inside `restart_telegram_bot`
    /// but the syscall blocks indefinitely. One leaked thread per stall
    /// episode is the accepted cost.
    fn check_telegram_watchdog(&mut self) {
        let Some(ref handle) = self.telegram else {
            return;
        };
        if self
            .last_telegram_respawn_at
            .is_some_and(|at| at.elapsed() < TELEGRAM_RESPAWN_COOLDOWN)
        {
            return;
        }
        let last_beat = handle.heartbeat.load(Ordering::Relaxed);
        if last_beat == 0 {
            // No cycle has completed yet — give the bot a chance to start.
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age_secs = now.saturating_sub(last_beat);
        if age_secs <= TELEGRAM_STALL_THRESHOLD_SECS {
            return;
        }
        tracing::warn!(age_secs, "telegram bot stalled, respawning");
        self.restart_telegram_bot();
        self.last_telegram_respawn_at = Some(Instant::now());
    }

    fn restart_telegram_bot(&mut self) {
        // Stop existing bot if running
        if let Some(ref h) = self.telegram {
            h.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.telegram = None;

        // Start new bot if both settings are configured
        let (token, chat_id) = use_cases::load_telegram_config(&self.config);
        match (token, chat_id) {
            (Some(token), Some(chat_id)) => {
                self.telegram = agman::telegram::start(&self.config, token, chat_id);
                if self.telegram.is_some() {
                    tracing::info!("telegram bot restarted with new settings");
                } else {
                    tracing::info!("telegram bot disabled (token or chat_id empty)");
                }
            }
            _ => {
                tracing::info!("telegram bot disabled (token or chat_id not set)");
            }
        }
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
        self.refresh_tasks_for_project();
        if let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == task_id) {
            self.selected_index = idx;
        }
    }

    pub fn refresh_projects(&mut self) {
        self.projects = use_cases::list_projects(&self.config).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to list projects");
            Vec::new()
        });
        // Sort held projects to the bottom (stable sort preserves alphabetical order within groups)
        self.projects.sort_by_key(|p| p.meta.held);
        // Count tasks per project and unassigned
        let all_tasks = Task::list_all(&self.config);
        self.project_task_counts.clear();
        self.unassigned_task_count = 0;
        self.unassigned_unseen_stopped_count = 0;
        for task in &all_tasks {
            if let Some(ref proj) = task.meta.project {
                let entry = self
                    .project_task_counts
                    .entry(proj.clone())
                    .or_insert((0, 0, 0));
                entry.0 += 1;
                if task.meta.status == TaskStatus::Running {
                    entry.1 += 1;
                }
                if !task.meta.seen && task.meta.status == TaskStatus::Stopped {
                    entry.2 += 1;
                }
            } else {
                self.unassigned_task_count += 1;
                if !task.meta.seen && task.meta.status == TaskStatus::Stopped {
                    self.unassigned_unseen_stopped_count += 1;
                }
            }
        }
        // Clamp selection
        let total = self.project_list_len();
        if self.selected_project_index >= total && total > 0 {
            self.selected_project_index = total - 1;
        }
    }

    /// Refresh the researcher list, filtered by `current_project` if set.
    pub fn refresh_researchers(&mut self) {
        self.researchers =
            use_cases::list_researchers(&self.config, self.current_project.as_deref())
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to list researchers");
                    Vec::new()
                });
        if self.researcher_list_index >= self.researchers.len() && !self.researchers.is_empty() {
            self.researcher_list_index = self.researchers.len() - 1;
        }
    }

    /// Total entries in the project list (projects + unassigned pseudo-entry).
    pub fn project_list_len(&self) -> usize {
        self.projects.len() + if self.unassigned_task_count > 0 { 1 } else { 0 }
    }

    /// Refresh tasks filtered by current_project.
    fn refresh_tasks_for_project(&mut self) {
        let prev_task_id = self.selected_task().map(|t| t.meta.task_id());
        let all = Task::list_all(&self.config);
        self.tasks = match &self.current_project {
            Some(name) if name == "(unassigned)" => all
                .into_iter()
                .filter(|t| t.meta.project.is_none())
                .collect(),
            Some(name) => all
                .into_iter()
                .filter(|t| t.meta.project.as_deref() == Some(name.as_str()))
                .collect(),
            None => all,
        };
        // Restore selection
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

    /// Process any stranded queue items for stopped tasks.
    /// This is a safety net: if items were queued while a task was running and
    /// the supervisor's relaunch failed to drain them (half-state), the TUI
    /// picks them up and routes through the supervisor: ensure tmux →
    /// wake_if_idle drains the queue and launches the next flow step in the
    /// `agman` window.
    pub fn process_stranded_queue(&mut self) {
        let stranded: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.meta.status == TaskStatus::Stopped && t.has_queued_items())
            .map(|t| t.meta.task_id())
            .collect();

        for task_id in stranded {
            let Some(idx) = self.tasks.iter().position(|t| t.meta.task_id() == task_id) else {
                continue;
            };

            self.log_output(format!(
                "Waking stopped task {} with queued items...",
                task_id
            ));

            if let Some(task) = self.tasks.get_mut(idx) {
                if let Err(e) = supervisor::ensure_task_tmux(task) {
                    self.log_output(format!("Failed to prepare tmux for {}: {}", task_id, e));
                    continue;
                }
                match supervisor::wake_if_idle(&self.config, task) {
                    Ok(Some(_)) => {
                        self.log_output(format!(
                            "Queued item drained and launched for {}",
                            task_id
                        ));
                        self.set_status(format!("Waking stranded task {}", task_id));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.log_output(format!("Error waking stranded task {}: {}", task_id, e));
                    }
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
        }
    }

    fn previous_task(&mut self) {
        if !self.tasks.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.tasks.len() - 1
            } else {
                self.selected_index - 1
            };
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

        self.preview_content = preview_content.clone();

        // Setup logs editor (read-only VimTextArea)
        self.logs_editor = VimTextArea::from_lines(preview_content.lines());
        self.logs_editor.set_read_only(true);
        self.logs_editor.set_normal_mode();
        self.logs_editor.move_cursor(CursorMove::Bottom);

        // Setup notes editor with vim mode (read-only until user starts editing)
        self.notes_content = notes_content.clone();
        self.notes_editor = VimTextArea::from_lines(notes_content.lines());
        self.notes_editor.set_read_only(true);
        self.notes_editor.set_normal_mode();
        self.notes_editor.move_cursor(CursorMove::Bottom);
        self.notes_editor.move_cursor(CursorMove::End);
        self.notes_editing = false;

        // Setup task file content for modal (editor gets set up when modal opens)
        self.task_file_content = task_file_content;

        // Mark stopped tasks as seen when the user opens preview
        if let Some(task) = self.tasks.get_mut(self.selected_index) {
            if task.meta.status == TaskStatus::Stopped && !task.meta.seen {
                if let Err(e) = use_cases::mark_task_seen(task) {
                    tracing::warn!(error = %e, "failed to mark task as seen");
                }
            }
        }
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
        let task_info = self
            .selected_task()
            .map(|t| (t.meta.task_id(), t.meta.status));

        if let Some((task_id, status)) = task_info {
            if status == TaskStatus::Stopped {
                self.set_status(format!("Task already stopped: {}", task_id));
                return Ok(());
            }

            tracing::info!(task_id = %task_id, "TUI: stop task requested");
            self.log_output(format!("Stopping task {}...", task_id));

            // Delegate business logic to use_cases. `stop_task` routes through
            // `supervisor::honor_stop`, which kills the live harness in the
            // agman pane via `/exit` (with Ctrl+C fallback), finalizes the
            // session, and restores any pre-command flow snapshot.
            let config = self.config.clone();
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                if let Err(e) = use_cases::stop_task(&config, task) {
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

    fn resume_after_answering(&mut self) -> Result<()> {
        let task_info = self
            .selected_task()
            .map(|t| (t.meta.task_id(), t.meta.status));

        if let Some((task_id, status)) = task_info {
            if status != TaskStatus::InputNeeded {
                return Ok(());
            }

            tracing::info!(task_id = %task_id, "TUI: resume after answering");

            let launch_error: Option<anyhow::Error> =
                if let Some(task) = self.tasks.get_mut(self.selected_index) {
                    use_cases::resume_after_answering(task)?;
                    let _ = use_cases::set_review_addressed(task, false);
                    supervisor::ensure_task_tmux(task)
                        .and_then(|_| supervisor::launch_next_step(&self.config, task).map(|_| ()))
                        .err()
                } else {
                    None
                };

            match launch_error {
                None => {
                    self.log_output(format!(
                        "Resumed flow for {} — processing your answers",
                        task_id
                    ));
                    self.set_status(format!("Resumed: {}", task_id));
                }
                Some(e) => {
                    tracing::error!(task_id = %task_id, error = %e, "failed to resume via supervisor");
                    self.log_output(format!("Resume failed: {}", e));
                    self.set_status(format!("Resume failed: {}", e));
                }
            }
            self.refresh_tasks_and_select(&task_id);
        }

        Ok(())
    }

    fn archive_task(&mut self, saved: bool) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let mut task = self.tasks.remove(self.selected_index);
        let task_id = task.meta.task_id();

        tracing::info!(task_id = %task_id, saved, "TUI: archive task requested");
        self.log_output(format!("Archiving task {}...", task_id));

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
        use_cases::archive_task(&self.config, &mut task, saved)?;
        self.log_output("  Archived task".to_string());

        if self.selected_index >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_index = self.tasks.len() - 1;
        }

        let label = if saved {
            "Archived & saved"
        } else {
            "Archived"
        };
        self.set_status(format!("{}: {}", label, task_id));
        self.view = View::TaskList;
        Ok(())
    }

    fn fully_delete_task(&mut self) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let task = self.tasks.remove(self.selected_index);
        let task_id = task.meta.task_id();

        tracing::info!(task_id = %task_id, "TUI: full delete requested");
        self.log_output(format!("Fully deleting task {}...", task_id));

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
        use_cases::fully_delete_task(&self.config, task)?;
        self.log_output("  Deleted task".to_string());

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
                let queue_count = use_cases::queue_feedback(task, &self.config, &feedback)?;
                self.log_output(format!(
                    "Queued feedback for {} ({} in queue)",
                    task_id, queue_count
                ));
                self.set_status(format!("Feedback queued ({} in queue)", queue_count));
            }
        } else if let Some(task) = self.tasks.get_mut(self.selected_index) {
            // Clear review_addressed on user interaction
            let _ = use_cases::set_review_addressed(task, false);

            // Ensure the task's tmux session + agman window exist before the
            // supervisor tries to send keys into them.
            if let Err(e) = supervisor::ensure_task_tmux(task) {
                self.log_output(format!("Failed to prepare tmux for {}: {}", task_id, e));
                self.set_status(format!("Feedback failed: {}", e));
                self.feedback_editor = VimTextArea::new();
                self.view = View::Preview;
                self.load_preview();
                return Ok(());
            }

            // Queue the feedback; wake_if_idle drains it (writes FEEDBACK.md,
            // switches to `continue` flow, flips to Running) and launches the
            // first step in the task's agman tmux window.
            let queue_count = use_cases::queue_feedback(task, &self.config, &feedback)?;
            self.log_output(format!(
                "Queued feedback for {} ({} in queue); supervisor waking",
                task_id, queue_count
            ));
            self.set_status(format!("Feedback submitted for {}", task_id));
            self.refresh_tasks_and_select(&task_id);
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
            self.set_status(format!(
                "No repos found in {}. Pick a repos directory (s to select, h/l to navigate).",
                self.config.repos_dir.display()
            ));
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
    fn create_wizard_from_picker(
        &mut self,
        repo_name: String,
        repo_path: PathBuf,
        is_multi: bool,
    ) -> Result<()> {
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
            let branches = self.scan_branches(&repo_name, &repo_path)?;
            let worktrees = self.scan_existing_worktrees(&repo_name, &repo_path)?;
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
    fn create_review_wizard_from_picker(
        &mut self,
        repo_name: String,
        repo_path: PathBuf,
    ) -> Result<()> {
        let branches = self.scan_branches(&repo_name, &repo_path)?;
        let worktrees = self.scan_existing_worktrees(&repo_name, &repo_path)?;

        let mut branch_editor = Self::create_plain_editor();
        branch_editor.set_cursor_line_style(ratatui::style::Style::default());

        self.review_wizard = Some(ReviewWizard {
            step: ReviewWizardStep::EnterBranch,
            selected_repo: repo_name,
            selected_repo_path: repo_path,
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

    fn scan_rebase_branches(&self, repo_path: &Path, local_only: bool) -> Result<Vec<String>> {
        // Get local branches
        let output = Command::new("git")
            .current_dir(repo_path)
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
                .current_dir(repo_path)
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
                    branches.push(branch.to_string());
                }
            }
        }

        branches.sort();
        branches.dedup();

        tracing::debug!(repo = %repo_path.display(), count = branches.len(), local_only, "scanned branches for picker");

        Ok(branches)
    }

    fn open_branch_picker(&mut self) {
        let (_repo_name, repo_path) = match self.selected_task() {
            Some(t) => {
                if t.meta.is_multi_repo() {
                    self.set_status("Branch picker not supported for multi-repo tasks".to_string());
                    return;
                }
                if !t.meta.has_repos() {
                    self.set_status("Task has no repos configured yet".to_string());
                    return;
                }
                let name = t.meta.primary_repo().repo_name.clone();
                let path = self
                    .config
                    .repo_path_for(t.meta.parent_dir.as_deref(), &name);
                (name, path)
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

        match self.scan_rebase_branches(&repo_path, local_only) {
            Ok(branches) => {
                if branches.is_empty() {
                    self.set_status("No branches found".to_string());
                    return;
                }

                // Preselect sensible default branch based on command
                let cmd_id = self.pending_branch_command.as_ref().map(|c| c.id.as_str());
                let preselect_index = match cmd_id {
                    Some("local-merge") => branches
                        .iter()
                        .position(|b| b == "main" || b == "master")
                        .unwrap_or(0),
                    Some("rebase") => branches
                        .iter()
                        .position(|b| b == "origin/main")
                        .or_else(|| branches.iter().position(|b| b == "origin/master"))
                        .or_else(|| branches.iter().position(|b| b == "main"))
                        .or_else(|| branches.iter().position(|b| b == "master"))
                        .unwrap_or(0),
                    _ => 0,
                };

                self.rebase_branches = branches;
                self.selected_rebase_branch_index = preselect_index;
                self.rebase_branch_list_state.select(Some(preselect_index));
                self.rebase_branch_search = Self::create_plain_editor();
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

        // If task is running, queue the command instead of running it immediately
        let is_running = self
            .selected_task()
            .map(|t| t.meta.status == TaskStatus::Running)
            .unwrap_or(false);
        if is_running {
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                match use_cases::queue_command(task, &self.config, &command.id, Some(branch)) {
                    Ok(count) => {
                        self.set_status(format!(
                            "Command queued: {} → {} ({} in queue)",
                            command.name, branch, count
                        ));
                    }
                    Err(e) => {
                        self.set_status(format!("Failed to queue command: {}", e));
                    }
                }
            }
            self.view = View::Preview;
            return Ok(());
        }

        self.log_output(format!(
            "Running '{}' with branch '{}' for task {}...",
            command.name, branch, task_id
        ));

        if let Some(task) = self.tasks.get_mut(self.selected_index) {
            if let Err(e) = supervisor::ensure_task_tmux(task) {
                self.log_output(format!("Failed to prepare tmux for {}: {}", task_id, e));
                self.set_status(format!("Failed to run {}: {}", command.name, e));
                self.view = View::Preview;
                self.load_preview();
                return Ok(());
            }
            match use_cases::queue_command(task, &self.config, &command.id, Some(branch)) {
                Ok(_count) => {
                    self.set_status(format!("Started: {} onto {}", command.name, branch));
                }
                Err(e) => {
                    self.log_output(format!("Failed: {}", e));
                    self.set_status(format!("Failed to run {}: {}", command.name, e));
                }
            }
        }

        self.refresh_tasks_and_select(&task_id);
        self.view = View::Preview;
        self.load_preview();
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

        // If task is running, queue the command instead of running it immediately
        let is_running = self
            .selected_task()
            .map(|t| t.meta.status == TaskStatus::Running)
            .unwrap_or(false);
        if is_running {
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                match use_cases::queue_command(task, &self.config, &command.id, None) {
                    Ok(count) => {
                        self.set_status(format!(
                            "Command queued: {} ({} in queue)",
                            command.name, count
                        ));
                    }
                    Err(e) => {
                        self.set_status(format!("Failed to queue command: {}", e));
                    }
                }
            }
            self.view = View::Preview;
            return Ok(());
        }

        // Guard: refuse create-pr if a PR is already linked
        if command.id == "create-pr" {
            if let Some(task) = self.selected_task() {
                if let Some(ref pr) = task.meta.linked_pr {
                    self.set_status(format!(
                        "PR #{} already linked — use monitor-pr instead.",
                        pr.number
                    ));
                    self.view = View::Preview;
                    return Ok(());
                }
            }
        }

        self.log_output(format!(
            "Running command '{}' on task {}...",
            command.name, task_id
        ));

        if let Some(task) = self.tasks.get_mut(self.selected_index) {
            if let Err(e) = supervisor::ensure_task_tmux(task) {
                self.log_output(format!("Failed to prepare tmux for {}: {}", task_id, e));
                self.set_status(format!("Failed to run {}: {}", command.name, e));
                self.view = View::Preview;
                self.load_preview();
                return Ok(());
            }
            match use_cases::queue_command(task, &self.config, &command.id, None) {
                Ok(_count) => {
                    if command.id == "address-review" {
                        let _ = use_cases::set_review_addressed(task, true);
                    } else {
                        let _ = use_cases::set_review_addressed(task, false);
                    }
                    self.set_status(format!("Started: {}", command.name));
                }
                Err(e) => {
                    self.log_output(format!("Failed: {}", e));
                    self.set_status(format!("Failed to run {}: {}", command.name, e));
                }
            }
        }

        self.refresh_tasks_and_select(&task_id);
        self.view = View::Preview;
        self.load_preview();
        Ok(())
    }

    fn scan_branches(&self, repo_name: &str, repo_path: &Path) -> Result<Vec<String>> {
        let output = Command::new("git")
            .current_dir(repo_path)
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

    fn scan_existing_worktrees(
        &self,
        repo_name: &str,
        repo_path: &Path,
    ) -> Result<Vec<(String, PathBuf)>> {
        let repo_path_buf = repo_path.to_path_buf();
        let worktrees = Git::list_worktrees(&repo_path_buf)?;

        // Build set of branches that already have tasks for this repo
        let existing_tasks: std::collections::HashSet<String> = self
            .tasks
            .iter()
            .filter(|t| t.meta.name == repo_name)
            .map(|t| t.meta.branch_name.clone())
            .collect();

        let main_repo_path = repo_path.canonicalize().unwrap_or(repo_path_buf);

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
                let base = wizard
                    .base_branch_editor
                    .lines()
                    .join("")
                    .trim()
                    .to_string();
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

        // Determine project assignment from current scope
        let project = self
            .current_project
            .as_ref()
            .filter(|p| p.as_str() != "(unassigned)")
            .cloned();

        tracing::info!(name = %name, branch = %branch_name, is_multi, "creating task via wizard");
        self.log_output(format!("Creating task {}--{}...", name, branch_name));

        if is_multi {
            // Multi-repo path: use the path directly from the directory picker
            let parent_dir = repo_path;
            let mut task = match use_cases::create_multi_repo_task(
                &self.config,
                &name,
                &branch_name,
                &description,
                "new-multi",
                parent_dir.clone(),
                review_after,
                project,
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

            let task_id = task.meta.task_id();
            self.log_output("  Launching flow via supervisor...".to_string());
            if let Err(e) = supervisor::ensure_task_tmux(&task)
                .and_then(|_| supervisor::launch_next_step(&self.config, &mut task).map(|_| ()))
            {
                tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to launch multi-repo task flow");
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to launch flow: {}", e));
                }
                return Ok(());
            }

            // Success - close wizard and refresh
            self.wizard = None;
            self.view = View::TaskList;
            if self.current_project.is_some() {
                self.refresh_tasks_for_project();
            } else {
                self.refresh_tasks_and_select(&task_id);
            }
            self.set_status(format!("Created multi-repo task: {}", task_id));
        } else {
            // Single-repo path: compute parent_dir when repo is outside repos_dir
            let parent_dir = repo_path.parent().and_then(|p| {
                if p != self.config.repos_dir {
                    Some(p.to_path_buf())
                } else {
                    None
                }
            });

            let mut task = match use_cases::create_task(
                &self.config,
                &name,
                &branch_name,
                &description,
                "new",
                worktree_source,
                review_after,
                parent_dir,
                project,
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

            let task_id = task.meta.task_id();
            self.log_output("  Launching flow via supervisor...".to_string());
            if let Err(e) = supervisor::ensure_task_tmux(&task)
                .and_then(|_| supervisor::launch_next_step(&self.config, &mut task).map(|_| ()))
            {
                tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to launch task flow");
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to launch flow: {}", e));
                }
                return Ok(());
            }

            // Success - close wizard and refresh
            self.wizard = None;
            self.view = View::TaskList;
            if self.current_project.is_some() {
                self.refresh_tasks_for_project();
            } else {
                self.refresh_tasks_and_select(&task_id);
            }
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
        let repo_path = wizard.selected_repo_path.clone();

        let (branch_name, worktree_source) = match wizard.branch_source {
            BranchSource::ExistingWorktree => {
                let (branch, path) =
                    wizard.existing_worktrees[wizard.selected_worktree_index].clone();
                (branch, use_cases::WorktreeSource::ExistingWorktree(path))
            }
            BranchSource::NewBranch => {
                let name = wizard.new_branch_editor.lines().join("").trim().to_string();
                let base = wizard
                    .base_branch_editor
                    .lines()
                    .join("")
                    .trim()
                    .to_string();
                let base_branch = if base.is_empty() { None } else { Some(base) };
                (name, use_cases::WorktreeSource::NewBranch { base_branch })
            }
            BranchSource::ExistingBranch => {
                let name = wizard.existing_branches[wizard.selected_branch_index].clone();
                (name, use_cases::WorktreeSource::ExistingBranch)
            }
        };

        // Compute parent_dir when repo is outside repos_dir
        let parent_dir = repo_path.parent().and_then(|p| {
            if p != self.config.repos_dir {
                Some(p.to_path_buf())
            } else {
                None
            }
        });

        // Determine project assignment from current scope
        let project = self
            .current_project
            .as_ref()
            .filter(|p| p.as_str() != "(unassigned)")
            .cloned();

        tracing::info!(repo = %repo_name, branch = %branch_name, "creating setup-only task via wizard");
        self.log_output(format!(
            "Creating setup-only task {}--{}...",
            repo_name, branch_name
        ));

        // Delegate business logic to use_cases
        let task = match use_cases::create_setup_only_task(
            &self.config,
            &repo_name,
            &branch_name,
            worktree_source,
            parent_dir,
            project,
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
        if let Err(e) = Tmux::create_session_with_windows(
            &task.meta.primary_repo().tmux_session,
            &worktree_path,
        ) {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        // No flow-run command sent — this is the key difference from create_task_from_wizard

        // Success - close wizard and refresh
        let task_id = task.meta.task_id();
        self.wizard = None;
        self.view = View::TaskList;
        if self.current_project.is_some() {
            self.refresh_tasks_for_project();
        } else {
            self.refresh_tasks_and_select(&task_id);
        }
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
            self.set_status(format!(
                "No repos found in {}. Pick a repos directory (s to select, h/l to navigate).",
                self.config.repos_dir.display()
            ));
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

                self.create_review_task()
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
        let repo_path = wizard.selected_repo_path.clone();
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

        // Compute parent_dir when repo is outside repos_dir
        let parent_dir = repo_path.parent().and_then(|p| {
            if p != self.config.repos_dir {
                Some(p.to_path_buf())
            } else {
                None
            }
        });

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
            parent_dir,
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
        if let Some(pr_number) =
            lookup_pr_for_branch(&task.meta.primary_repo().worktree_path, &branch_name)
        {
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
        if let Err(e) = Tmux::create_session_with_windows(
            &task.meta.primary_repo().tmux_session,
            &worktree_path,
        ) {
            self.log_output(format!("  Error: {}", e));
            if let Some(w) = &mut self.review_wizard {
                w.error_message = Some(format!("Failed to create tmux session: {}", e));
            }
            return Ok(());
        }

        let task_id = task.meta.task_id();
        let review_cmd = format!("agman run-command {} review-pr", task_id);
        let _ =
            Tmux::send_keys_to_window(&task.meta.primary_repo().tmux_session, "agman", &review_cmd);

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
            View::ProjectList => self.handle_project_list_event(event),
            View::TaskList => self.handle_task_list_event(event),
            View::Preview => self.handle_preview_event(event),
            View::DeleteConfirm => self.handle_delete_confirm_event(event),
            View::Feedback => self.handle_feedback_event(event),
            View::NewTaskWizard => self.handle_wizard_event(event),
            View::CommandList => self.handle_command_list_event(event),
            View::TaskEditor => self.handle_task_editor_event(event),
            View::Queue => self.handle_queue_event(event),
            View::RebaseBranchPicker => self.handle_rebase_branch_picker_event(event),
            View::ReviewWizard => self.handle_review_wizard_event(event),
            View::RestartWizard => self.handle_restart_wizard_event(event),
            View::DirectoryPicker => self.handle_directory_picker_event(event),
            View::SessionPicker => self.handle_session_picker_event(event),
            View::Notifications => self.handle_notifications_event(event),
            View::Notes => self.handle_notes_event(event),
            View::ShowPrs => self.handle_show_prs_event(event),
            View::Settings => self.handle_settings_event(event),
            View::Archive => self.handle_archive_event(event),
            View::ProjectWizard => self.handle_project_wizard_event(event),
            View::ProjectPicker => self.handle_project_picker_event(event),
            View::ProjectDeleteConfirm => self.handle_project_delete_confirm_event(event),
            View::ResearcherList => self.handle_researcher_list_event(event),
            View::ResearcherWizard => self.handle_researcher_wizard_event(event),
            View::RespawnConfirm => self.handle_respawn_confirm_event(event),
        }
    }

    fn handle_project_list_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Char('j') => {
                    let total = self.project_list_len();
                    if total > 0 {
                        self.selected_project_index = (self.selected_project_index + 1) % total;
                    }
                }
                KeyCode::Char('k') => {
                    let total = self.project_list_len();
                    if total > 0 {
                        self.selected_project_index = if self.selected_project_index == 0 {
                            total - 1
                        } else {
                            self.selected_project_index - 1
                        };
                    }
                }
                KeyCode::Char('g') => {
                    self.selected_project_index = 0;
                }
                KeyCode::Char('G') => {
                    let total = self.project_list_len();
                    if total > 0 {
                        self.selected_project_index = total - 1;
                    }
                }
                KeyCode::Enter | KeyCode::Char('l') => {
                    // Navigate into the selected project's task list
                    let project_name = if self.selected_project_index < self.projects.len() {
                        Some(self.projects[self.selected_project_index].meta.name.clone())
                    } else if self.unassigned_task_count > 0 {
                        // Last entry is the "(unassigned)" pseudo-project
                        Some("(unassigned)".to_string())
                    } else {
                        None
                    };
                    if let Some(name) = project_name {
                        self.current_project = Some(name);
                        self.selected_index = 0;
                        self.refresh_tasks_for_project();
                        self.view = View::TaskList;
                    }
                }
                KeyCode::Char('c') => {
                    // Open Chief of Staff chat as a tmux popup (non-blocking)
                    if self.popup.is_some() {
                        return Ok(false);
                    }
                    match use_cases::open_chief_of_staff_popup(&self.config) {
                        Ok(child) => {
                            tracing::info!("opened Chief of Staff popup");
                            self.popup = Some(ActivePopup { child });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to open Chief of Staff popup");
                            self.set_status(format!("Failed to open Chief of Staff chat: {e}"));
                        }
                    }
                }
                KeyCode::Char('n') => {
                    let mut name_editor = TextArea::default();
                    name_editor.set_cursor_line_style(ratatui::style::Style::default());
                    name_editor.set_placeholder_text("project-name");
                    self.project_wizard = Some(ProjectWizard {
                        name_editor,
                        description_editor: {
                            let mut ed = VimTextArea::new();
                            ed.set_insert_mode();
                            ed
                        },
                        description_focus: false,
                        error_message: None,
                    });
                    self.view = View::ProjectWizard;
                }
                KeyCode::Char('m') => {
                    // Migrate all unassigned tasks — only available when (unassigned) is selected
                    let is_unassigned = self.selected_project_index >= self.projects.len()
                        && self.unassigned_task_count > 0;
                    if is_unassigned {
                        let project_names: Vec<String> =
                            self.projects.iter().map(|p| p.meta.name.clone()).collect();
                        if project_names.is_empty() {
                            self.set_status("Create a project first with 'n'".to_string());
                        } else {
                            self.project_picker = Some(ProjectPicker {
                                projects: project_names,
                                selected: 0,
                                action: ProjectPickerAction::MigrateAllUnassigned,
                            });
                            self.view = View::ProjectPicker;
                        }
                    }
                }
                KeyCode::Char('d') => {
                    // Delete project — only for real projects, not "(unassigned)"
                    if self.selected_project_index < self.projects.len() {
                        let name = self.projects[self.selected_project_index].meta.name.clone();
                        self.project_to_delete = Some(name);
                        self.view = View::ProjectDeleteConfirm;
                    }
                }
                KeyCode::Char('h') => {
                    // Toggle hold — only for real projects, not "(unassigned)"
                    if self.selected_project_index < self.projects.len() {
                        let name = self.projects[self.selected_project_index].meta.name.clone();
                        match use_cases::toggle_project_hold(&self.config, &name) {
                            Ok(()) => {
                                // In-memory state is still pre-toggle (refresh hasn't happened yet)
                                let was_held = self.projects[self.selected_project_index].meta.held;
                                let msg = if was_held {
                                    format!("Resumed: {name}")
                                } else {
                                    format!("On hold: {name}")
                                };
                                tracing::info!(project = %name, "toggled project hold");
                                self.refresh_projects();
                                self.set_status(msg);
                            }
                            Err(e) => {
                                tracing::error!(project = %name, error = %e, "failed to toggle project hold");
                                self.set_status(format!("Failed to toggle hold: {e}"));
                            }
                        }
                    }
                }
                KeyCode::Char('o') => match NotesView::new(self.config.notes_dir.clone()) {
                    Ok(nv) => {
                        tracing::info!("opening notes view");
                        self.notes_view = Some(nv);
                        self.view = View::Notes;
                    }
                    Err(e) => {
                        self.set_status(format!("Failed to open notes: {e}"));
                    }
                },
                KeyCode::Char('i') => {
                    self.selected_notif_index = 0;
                    self.view = View::Notifications;
                }
                KeyCode::Char('p') => {
                    self.show_prs_selected = 0;
                    self.view = View::ShowPrs;
                    if !self.show_prs_first_poll_done && !self.show_prs_poll_active {
                        self.start_show_prs_poll();
                    }
                }
                KeyCode::Char('e') => {
                    // Show respawn confirmation for Chief of Staff
                    if self.respawn_in_progress.is_none() {
                        self.respawn_confirm_target = Some("chief-of-staff".to_string());
                        self.respawn_confirm_index = 0;
                        self.respawn_confirm_is_chief_of_staff = true;
                        self.respawn_confirm_return_view = View::ProjectList;
                        self.view = View::RespawnConfirm;
                    }
                }
                KeyCode::Char('w') => {
                    self.current_project = Some("chief-of-staff".to_string());
                    self.researcher_list_index = 0;
                    self.refresh_researchers();
                    self.view = View::ResearcherList;
                }
                KeyCode::Char(',') => {
                    self.settings_selected = 0;
                    self.view = View::Settings;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_project_delete_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Enter => {
                    if let Some(name) = self.project_to_delete.take() {
                        match use_cases::delete_project(&self.config, &name) {
                            Ok(()) => {
                                self.refresh_projects();
                                if self.selected_project_index >= self.project_list_len()
                                    && self.project_list_len() > 0
                                {
                                    self.selected_project_index = self.project_list_len() - 1;
                                }
                                self.set_status(format!("Deleted project '{}'", name));
                            }
                            Err(e) => {
                                tracing::error!(project = %name, error = %e, "failed to delete project");
                                self.set_status(format!("Delete failed: {e}"));
                            }
                        }
                    }
                    self.view = View::ProjectList;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.project_to_delete = None;
                    self.view = View::ProjectList;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_task_list_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.current_project = None;
                    self.refresh_projects();
                    self.view = View::ProjectList;
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
                KeyCode::Char('s') => {
                    self.stop_task()?;
                }
                KeyCode::Char('d') => {
                    if !self.tasks.is_empty() {
                        self.archive_mode_index = 0;
                        self.view = View::DeleteConfirm;
                    }
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
                KeyCode::Char('v') => {
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
                        t.meta
                            .linked_pr
                            .as_ref()
                            .map(|pr| (pr.number, pr.url.clone()))
                    });
                    if let Some((number, url)) = pr_info {
                        open_url(&url);
                        self.set_status(format!("Opening PR #{}...", number));
                    } else {
                        self.set_status("No linked PR".to_string());
                    }
                }
                KeyCode::Char('r') => {
                    // Rerun task wizard
                    self.start_restart_wizard()?;
                }
                KeyCode::Char('h') => {
                    self.toggle_hold()?;
                }
                KeyCode::Char('c') => {
                    if let Some(ref project_name) = self.current_project.clone() {
                        if project_name != "(unassigned)" {
                            // Open PM chat as a tmux popup (non-blocking)
                            if self.popup.is_some() {
                                return Ok(false);
                            }
                            match use_cases::open_pm_popup(&self.config, project_name) {
                                Ok(child) => {
                                    tracing::info!(project = %project_name, "opened PM popup");
                                    self.popup = Some(ActivePopup { child });
                                }
                                Err(e) => {
                                    tracing::error!(project = %project_name, error = %e, "failed to open PM popup");
                                    self.set_status(format!("Failed to open PM chat: {e}"));
                                }
                            }
                        }
                    } else {
                        // Original behavior: toggle review_addressed
                        let is_owned = self
                            .selected_task()
                            .and_then(|t| t.meta.linked_pr.as_ref().map(|pr| pr.owned))
                            .unwrap_or(false);
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
                }
                KeyCode::Char('z') => {
                    self.archive_tasks = use_cases::list_archived_tasks(&self.config);
                    self.archive_search = Self::create_plain_editor();
                    self.archive_selected = 0;
                    self.archive_preview = None;
                    self.archive_scroll = 0;
                    self.view = View::Archive;
                }
                KeyCode::Char('e') => {
                    // Show respawn confirmation for PM
                    if let Some(ref project_name) = self.current_project.clone() {
                        if project_name != "(unassigned)" && self.respawn_in_progress.is_none() {
                            self.respawn_confirm_target = Some(project_name.clone());
                            self.respawn_confirm_index = 0;
                            self.respawn_confirm_is_chief_of_staff = false;
                            self.respawn_confirm_return_view = View::TaskList;
                            self.view = View::RespawnConfirm;
                        }
                    }
                }
                KeyCode::Char('w') => {
                    let project_name = if self.selected_project_index < self.projects.len() {
                        Some(self.projects[self.selected_project_index].meta.name.clone())
                    } else {
                        None // "(unassigned)" pseudo-project — no researchers to show
                    };
                    if let Some(name) = project_name {
                        self.current_project = Some(name);
                        self.researcher_list_index = 0;
                        self.refresh_researchers();
                        self.view = View::ResearcherList;
                    } else {
                        self.set_status("No researchers for unassigned tasks".to_string());
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_researcher_list_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.current_project = None;
                    self.refresh_projects();
                    self.view = View::ProjectList;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.researchers.is_empty()
                        && self.researcher_list_index < self.researchers.len() - 1
                    {
                        self.researcher_list_index += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if self.researcher_list_index > 0 {
                        self.researcher_list_index -= 1;
                    }
                }
                KeyCode::Enter => {
                    // Attach to researcher tmux session, resuming archived ones
                    if self.popup.is_some() {
                        return Ok(false);
                    }
                    if let Some(researcher) = self.researchers.get(self.researcher_list_index) {
                        let project = researcher.meta.project.clone();
                        let name = researcher.meta.name.clone();
                        let session_name = Config::researcher_tmux_session(&project, &name);

                        if !Tmux::session_exists(&session_name) {
                            // Session not running — resume it (works for both archived and crashed running sessions)
                            match use_cases::resume_researcher(&self.config, &project, &name) {
                                Ok(()) => {
                                    tracing::info!(
                                        session = &session_name,
                                        "resumed researcher session"
                                    );
                                    self.refresh_researchers();
                                }
                                Err(e) => {
                                    self.set_status(format!("Failed to resume: {e}"));
                                    return Ok(false);
                                }
                            }
                        }

                        match Tmux::popup_attach(&session_name) {
                            Ok(child) => {
                                tracing::info!(
                                    session = &session_name,
                                    "attached to researcher session"
                                );
                                self.popup = Some(ActivePopup { child });
                            }
                            Err(e) => {
                                self.set_status(format!("Failed to attach: {e}"));
                            }
                        }
                    }
                }
                KeyCode::Char('n') => {
                    // Create new researcher
                    let project = if let Some(ref p) = self.current_project {
                        p.clone()
                    } else if let Some(first) = self.projects.first() {
                        first.meta.name.clone()
                    } else {
                        self.set_status("No project available".to_string());
                        return Ok(false);
                    };
                    tracing::info!(project = %project, "opening researcher wizard");
                    let mut name_editor = TextArea::default();
                    name_editor.set_cursor_line_style(ratatui::style::Style::default());
                    self.researcher_wizard = Some(ResearcherWizard {
                        name_editor,
                        description_editor: VimTextArea::new(),
                        description_focus: false,
                        error_message: None,
                        project,
                    });
                    self.view = View::ResearcherWizard;
                }
                KeyCode::Char('d') => {
                    // Archive selected researcher
                    if let Some(researcher) = self.researchers.get(self.researcher_list_index) {
                        let project = researcher.meta.project.clone();
                        let name = researcher.meta.name.clone();
                        match use_cases::archive_researcher(&self.config, &project, &name) {
                            Ok(()) => {
                                self.set_status(format!("Archived researcher '{name}'"));
                                self.refresh_researchers();
                                if self.researcher_list_index >= self.researchers.len()
                                    && !self.researchers.is_empty()
                                {
                                    self.researcher_list_index = self.researchers.len() - 1;
                                }
                            }
                            Err(e) => {
                                self.set_status(format!("Failed to archive: {e}"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_settings_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // When editing a text field, route keys to the TextArea
            if self.settings_editing {
                match key.code {
                    KeyCode::Enter => {
                        // Save the edited value
                        let token_text: String = self.telegram_token_editor.lines().join("");
                        let chat_id_text: String = self.telegram_chat_id_editor.lines().join("");
                        let token = if token_text.is_empty() {
                            None
                        } else {
                            Some(token_text)
                        };
                        let chat_id = if chat_id_text.is_empty() {
                            None
                        } else {
                            Some(chat_id_text)
                        };
                        if let Err(e) =
                            use_cases::save_telegram_config(&self.config, token, chat_id)
                        {
                            tracing::error!(error = %e, "failed to save telegram config");
                            self.set_status(format!("Failed to save: {e}"));
                        } else {
                            self.set_status("Telegram settings saved".to_string());
                            self.restart_telegram_bot();
                        }
                        self.settings_editing = false;
                    }
                    KeyCode::Esc => {
                        // Discard edit, restore from config
                        let (token, chat_id) = use_cases::load_telegram_config(&self.config);
                        self.telegram_token_editor = TextArea::new(vec![token.unwrap_or_default()]);
                        self.telegram_token_editor
                            .set_cursor_line_style(ratatui::style::Style::default());
                        self.telegram_chat_id_editor =
                            TextArea::new(vec![chat_id.unwrap_or_default()]);
                        self.telegram_chat_id_editor
                            .set_cursor_line_style(ratatui::style::Style::default());
                        self.settings_editing = false;
                    }
                    _ => match self.settings_selected {
                        2 => {
                            self.telegram_token_editor.input(event);
                        }
                        3 => {
                            self.telegram_chat_id_editor.input(event);
                        }
                        _ => {}
                    },
                }
                return Ok(false);
            }

            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if self.settings_selected < 3 {
                        self.settings_selected += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if self.settings_selected > 0 {
                        self.settings_selected -= 1;
                    }
                }
                KeyCode::Enter => {
                    // Enter editing mode for text fields
                    if self.settings_selected == 2 || self.settings_selected == 3 {
                        self.settings_editing = true;
                        // Move cursor to end of current value
                        let editor = if self.settings_selected == 2 {
                            &mut self.telegram_token_editor
                        } else {
                            &mut self.telegram_chat_id_editor
                        };
                        let line_len = editor.lines().first().map(|l| l.len()).unwrap_or(0);
                        editor.move_cursor(tui_textarea::CursorMove::Jump(0, line_len as u16));
                    }
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    match self.settings_selected {
                        0 => {
                            // Decrease archive retention by 7 days (min 7)
                            if self.archive_retention_days > 7 {
                                let new_days = self.archive_retention_days - 7;
                                tracing::info!(
                                    old_days = self.archive_retention_days,
                                    new_days,
                                    "archive retention changed"
                                );
                                self.archive_retention_days = new_days;
                                if let Err(e) =
                                    use_cases::save_archive_retention(&self.config, new_days)
                                {
                                    tracing::error!(error = %e, "failed to save archive retention");
                                    self.set_status(format!("Failed to save: {e}"));
                                }
                            }
                        }
                        1 => {
                            // Cycle harness left through HarnessKind::ALL
                            self.cycle_harness(-1);
                        }
                        _ => {}
                    }
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    match self.settings_selected {
                        0 => {
                            // Increase archive retention by 7 days (max 365)
                            if self.archive_retention_days < 365 {
                                let new_days = self.archive_retention_days + 7;
                                tracing::info!(
                                    old_days = self.archive_retention_days,
                                    new_days,
                                    "archive retention changed"
                                );
                                self.archive_retention_days = new_days;
                                if let Err(e) =
                                    use_cases::save_archive_retention(&self.config, new_days)
                                {
                                    tracing::error!(error = %e, "failed to save archive retention");
                                    self.set_status(format!("Failed to save: {e}"));
                                }
                            }
                        }
                        1 => {
                            // Cycle harness right through HarnessKind::ALL
                            self.cycle_harness(1);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Cycle the configured harness by `delta` (typically -1 / +1).
    fn cycle_harness(&mut self, delta: i32) {
        use agman::harness::HarnessKind;
        let all = HarnessKind::ALL;
        let current = self.config.harness_kind();
        let pos = all.iter().position(|k| *k == current).unwrap_or(0);
        let new_pos = ((pos as i32 + delta).rem_euclid(all.len() as i32)) as usize;
        let new_kind = all[new_pos];
        if new_kind == current {
            return;
        }
        match use_cases::save_harness(&self.config, new_kind) {
            Ok(()) => {
                tracing::info!(harness = %new_kind, "harness setting changed");
                self.set_status(format!(
                    "Harness set to {} (applies to newly-spawned agents)",
                    new_kind
                ));
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to save harness");
                self.set_status(format!("Failed to save harness: {e}"));
            }
        }
    }

    /// Return indices into `self.archive_tasks` that match the current search query.
    pub fn archive_filtered_indices(&self) -> Vec<usize> {
        let query: String = self.archive_search.lines().join("").to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return (0..self.archive_tasks.len()).collect();
        }
        self.archive_tasks
            .iter()
            .enumerate()
            .filter(|(_, (task, content))| {
                let id_lower = task.meta.task_id().to_lowercase();
                let content_lower = content.to_lowercase();
                terms
                    .iter()
                    .all(|term| id_lower.contains(term) || content_lower.contains(term))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Return indices into `self.rebase_branches` that match the current search query.
    pub fn rebase_branch_filtered_indices(&self) -> Vec<usize> {
        let query: String = self.rebase_branch_search.lines().join("").to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return (0..self.rebase_branches.len()).collect();
        }
        let mut matched: Vec<usize> = self
            .rebase_branches
            .iter()
            .enumerate()
            .filter(|(_, branch)| {
                let branch_lower = branch.to_lowercase();
                terms.iter().all(|term| branch_lower.contains(term))
            })
            .map(|(i, _)| i)
            .collect();
        matched.sort_by(|&a, &b| {
            let score_a = branch_search_score(&self.rebase_branches[a], &terms);
            let score_b = branch_search_score(&self.rebase_branches[b], &terms);
            score_b.cmp(&score_a)
        });
        matched
    }

    fn handle_project_wizard_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            // Ctrl+S to submit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                if let Some(wizard) = &self.project_wizard {
                    let name = wizard.name_editor.lines().join("").trim().to_string();
                    let desc = wizard.description_editor.lines_joined();
                    let desc = desc.trim().to_string();

                    if name.is_empty() {
                        if let Some(w) = &mut self.project_wizard {
                            w.error_message = Some("Project name is required".to_string());
                        }
                        return Ok(false);
                    }

                    match use_cases::create_project(&self.config, &name, &desc, None) {
                        Ok(_project) => {
                            tracing::info!(project = %name, "created project via wizard");
                            self.set_status(format!("Created project: {name}"));
                            self.project_wizard = None;
                            self.view = View::ProjectList;
                            self.refresh_projects();
                        }
                        Err(e) => {
                            tracing::warn!(project = %name, error = %e, "failed to create project");
                            if let Some(w) = &mut self.project_wizard {
                                w.error_message = Some(format!("{e}"));
                            }
                        }
                    }
                }
                return Ok(false);
            }

            if let Some(wizard) = &mut self.project_wizard {
                wizard.error_message = None;
                let input = Input::from(event.clone());

                // Tab to switch focus between name and description
                if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
                    wizard.description_focus = !wizard.description_focus;
                    if wizard.description_focus {
                        wizard.description_editor.set_insert_mode();
                    }
                    return Ok(false);
                }

                if wizard.description_focus {
                    let was_insert = wizard.description_editor.mode() == VimMode::Insert;
                    wizard.description_editor.input(input.clone());
                    let is_normal_now = wizard.description_editor.mode() == VimMode::Normal;

                    // Esc in normal mode goes back to name field
                    if input.key == Key::Esc && !was_insert && is_normal_now {
                        wizard.description_focus = false;
                    }
                } else {
                    // Name field: Esc cancels wizard
                    if key.code == KeyCode::Esc {
                        self.project_wizard = None;
                        self.view = View::ProjectList;
                        return Ok(false);
                    }
                    // Enter in name field moves to description
                    if key.code == KeyCode::Enter {
                        wizard.description_focus = true;
                        wizard.description_editor.set_insert_mode();
                        return Ok(false);
                    }
                    wizard.name_editor.input(input);
                }
            }
        }
        Ok(false)
    }

    fn handle_researcher_wizard_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            // Ctrl+S to submit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                if let Some(wizard) = &self.researcher_wizard {
                    let name = wizard.name_editor.lines().join("").trim().to_string();
                    let desc = wizard.description_editor.lines_joined();
                    let desc = desc.trim().to_string();
                    let project = wizard.project.clone();

                    if name.is_empty() {
                        if let Some(w) = &mut self.researcher_wizard {
                            w.error_message = Some("Researcher name is required".to_string());
                        }
                        return Ok(false);
                    }

                    match use_cases::create_researcher(
                        &self.config,
                        &project,
                        &name,
                        &desc,
                        None,
                        None,
                        None,
                    ) {
                        Ok(_researcher) => {
                            tracing::info!(project = %project, name = %name, "created researcher via wizard");
                            // Start the session
                            if let Err(e) = use_cases::start_researcher_session(
                                &self.config,
                                &project,
                                &name,
                                false,
                            ) {
                                tracing::warn!(
                                    project = %project, name = %name, error = %e,
                                    "failed to start researcher session"
                                );
                            }
                            self.set_status(format!("Created researcher: {name}"));
                            self.researcher_wizard = None;
                            self.view = View::ResearcherList;
                            self.refresh_researchers();
                        }
                        Err(e) => {
                            tracing::warn!(project = %project, name = %name, error = %e, "failed to create researcher");
                            if let Some(w) = &mut self.researcher_wizard {
                                w.error_message = Some(format!("{e}"));
                            }
                        }
                    }
                }
                return Ok(false);
            }

            if let Some(wizard) = &mut self.researcher_wizard {
                wizard.error_message = None;
                let input = Input::from(event.clone());

                // Tab to switch focus between name and description
                if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
                    wizard.description_focus = !wizard.description_focus;
                    if wizard.description_focus {
                        wizard.description_editor.set_insert_mode();
                    }
                    return Ok(false);
                }

                if wizard.description_focus {
                    let was_insert = wizard.description_editor.mode() == VimMode::Insert;
                    wizard.description_editor.input(input.clone());
                    let is_normal_now = wizard.description_editor.mode() == VimMode::Normal;

                    // Esc in normal mode goes back to name field
                    if input.key == Key::Esc && !was_insert && is_normal_now {
                        wizard.description_focus = false;
                    }
                } else {
                    // Name field: Esc cancels wizard
                    if key.code == KeyCode::Esc {
                        self.researcher_wizard = None;
                        self.view = View::ResearcherList;
                        return Ok(false);
                    }
                    // Enter in name field moves to description
                    if key.code == KeyCode::Enter {
                        wizard.description_focus = true;
                        wizard.description_editor.set_insert_mode();
                        return Ok(false);
                    }
                    wizard.name_editor.input(input);
                }
            }
        }
        Ok(false)
    }

    fn handle_project_picker_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    let was_migrate_all = self.project_picker.as_ref().is_some_and(|p| {
                        matches!(p.action, ProjectPickerAction::MigrateAllUnassigned)
                    });
                    self.project_picker = None;
                    self.view = if was_migrate_all {
                        View::ProjectList
                    } else {
                        View::TaskList
                    };
                }
                KeyCode::Char('j') => {
                    if let Some(picker) = &mut self.project_picker {
                        if !picker.projects.is_empty() {
                            picker.selected = (picker.selected + 1) % picker.projects.len();
                        }
                    }
                }
                KeyCode::Char('k') => {
                    if let Some(picker) = &mut self.project_picker {
                        if !picker.projects.is_empty() {
                            picker.selected = if picker.selected == 0 {
                                picker.projects.len() - 1
                            } else {
                                picker.selected - 1
                            };
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(picker) = self.project_picker.take() {
                        if let Some(project_name) = picker.projects.get(picker.selected) {
                            let project_name = project_name.clone();
                            match &picker.action {
                                ProjectPickerAction::MigrateAllUnassigned => {
                                    let unassigned = use_cases::list_unassigned_tasks(&self.config)
                                        .unwrap_or_default();
                                    let task_ids: Vec<String> =
                                        unassigned.iter().map(|t| t.meta.task_id()).collect();
                                    match use_cases::migrate_tasks_to_project(
                                        &self.config,
                                        &project_name,
                                        &task_ids,
                                    ) {
                                        Ok(count) => {
                                            tracing::info!(
                                                project = %project_name,
                                                count,
                                                "migrated unassigned tasks"
                                            );
                                            self.set_status(format!(
                                                "Migrated {count} tasks to {project_name}"
                                            ));
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "failed to migrate tasks");
                                            self.set_status(format!("Migration failed: {e}"));
                                        }
                                    }
                                    self.refresh_projects();
                                    self.view = View::ProjectList;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_archive_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            if self.archive_preview.is_some() {
                // Preview mode
                return self.handle_archive_preview_event(key.code, key.modifiers);
            }

            // Search mode
            match key.code {
                KeyCode::Esc => {
                    self.view = View::TaskList;
                }
                KeyCode::Up | KeyCode::Down => {
                    let filtered = self.archive_filtered_indices();
                    if !filtered.is_empty() {
                        if key.code == KeyCode::Up {
                            self.archive_selected = self.archive_selected.saturating_sub(1);
                        } else {
                            self.archive_selected =
                                (self.archive_selected + 1).min(filtered.len() - 1);
                        }
                    }
                }
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let filtered = self.archive_filtered_indices();
                    if !filtered.is_empty() {
                        self.archive_selected = (self.archive_selected + 1).min(filtered.len() - 1);
                    }
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.archive_selected = self.archive_selected.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let filtered = self.archive_filtered_indices();
                    if let Some(&task_idx) = filtered.get(self.archive_selected) {
                        let content = self.archive_tasks[task_idx].1.clone();
                        self.archive_preview = Some(content);
                        self.archive_scroll = 0;
                    }
                }
                KeyCode::Backspace => {
                    self.archive_search.input(Input {
                        key: Key::Backspace,
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                    // Reset selection after search change
                    self.archive_selected = 0;
                }
                KeyCode::Char(c) => {
                    self.archive_search.input(Input {
                        key: Key::Char(c),
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                    // Reset selection after search change
                    self.archive_selected = 0;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_archive_preview_event(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<bool> {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.archive_preview = None;
            }
            KeyCode::Char('j') => {
                self.archive_scroll = self.archive_scroll.saturating_add(1);
            }
            KeyCode::Char('k') => {
                self.archive_scroll = self.archive_scroll.saturating_sub(1);
            }
            KeyCode::Char('g') => {
                self.archive_scroll = 0;
            }
            KeyCode::Char('G') => {
                // Jump to bottom — use a large value, clamped during rendering
                self.archive_scroll = u16::MAX;
            }
            KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.archive_scroll = self.archive_scroll.saturating_add(15);
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.archive_scroll = self.archive_scroll.saturating_sub(15);
            }
            KeyCode::Char('s') => {
                // Toggle saved
                let filtered = self.archive_filtered_indices();
                if let Some(&task_idx) = filtered.get(self.archive_selected) {
                    let task = &mut self.archive_tasks[task_idx].0;
                    match use_cases::toggle_archive_saved(&self.config, task) {
                        Ok(()) => {
                            let label = if task.meta.saved { "Saved" } else { "Unsaved" };
                            let task_id = task.meta.task_id();
                            self.set_status(format!("{}: {}", label, task_id));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to toggle archive saved");
                            self.set_status(format!("Failed to toggle saved: {e}"));
                        }
                    }
                }
            }
            KeyCode::Char('d') => {
                // Permanently delete
                let filtered = self.archive_filtered_indices();
                if let Some(&task_idx) = filtered.get(self.archive_selected) {
                    let (task, _) = self.archive_tasks.remove(task_idx);
                    let task_id = task.meta.task_id();
                    if let Err(e) = use_cases::permanently_delete_archived_task(&self.config, task)
                    {
                        tracing::error!(task_id = %task_id, error = %e, "failed to permanently delete archived task");
                        self.set_status(format!("Delete failed: {e}"));
                    } else {
                        self.set_status(format!("Deleted: {}", task_id));
                    }
                    self.archive_preview = None;
                    // Clamp selection
                    let filtered = self.archive_filtered_indices();
                    if self.archive_selected >= filtered.len() && !filtered.is_empty() {
                        self.archive_selected = filtered.len() - 1;
                    }
                }
            }
            KeyCode::Char('n') => {
                // New task from archived task
                let filtered = self.archive_filtered_indices();
                if let Some(&task_idx) = filtered.get(self.archive_selected) {
                    let (task, _content) = &self.archive_tasks[task_idx];
                    let repo_name = task.meta.name.clone();
                    let task_id = task.meta.task_id();
                    let branch_name = task.meta.branch_name.clone();
                    let repo_path = self
                        .config
                        .repo_path_for(task.meta.parent_dir.as_deref(), &repo_name);

                    if !repo_path.exists() {
                        self.set_status(format!("Repo path not found: {}", repo_path.display()));
                    } else {
                        tracing::info!(task_id = %task_id, repo = %repo_name, "starting new task from archived task");
                        self.archive_preview = None;
                        self.create_wizard_from_picker(repo_name, repo_path, false)?;

                        let prefill = format!(
                            "Reference: task \"{}\" (branch: {}). Examine ~/.agman/tasks/{}/TASK.md for context, and the git branch/PR if needed.\n\n\n",
                            task_id, branch_name, task_id,
                        );
                        if let Some(wizard) = self.wizard.as_mut() {
                            wizard.description_editor.textarea.insert_str(&prefill);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_preview_event(&mut self, event: Event) -> Result<bool> {
        // If editing notes, handle vim-style input with auto-wrap and save logic
        if self.notes_editing && self.preview_pane == PreviewPane::Notes {
            return self.handle_notes_editing(event);
        }

        if let Event::Key(key) = event {
            // Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            // Tab/BackTab to switch panes
            if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
                self.preview_pane = match self.preview_pane {
                    PreviewPane::Logs => PreviewPane::Notes,
                    PreviewPane::Notes => PreviewPane::Logs,
                };
                return Ok(false);
            }

            // Esc: forward to VimTextArea first (cancels Visual/Operator), then exit if already Normal
            if key.code == KeyCode::Esc {
                let editor = match self.preview_pane {
                    PreviewPane::Logs => &self.logs_editor,
                    PreviewPane::Notes => &self.notes_editor,
                };
                let was_normal = editor.mode() == VimMode::Normal;

                let input = Input::from(event.clone());
                match self.preview_pane {
                    PreviewPane::Logs => self.logs_editor.input(input),
                    PreviewPane::Notes => self.notes_editor.input(input),
                }

                if was_normal {
                    self.view = View::TaskList;
                }
                return Ok(false);
            }

            // q: exit preview if in Normal mode, otherwise forward to editor
            if key.code == KeyCode::Char('q') && !key.modifiers.contains(KeyModifiers::CONTROL) {
                let editor = match self.preview_pane {
                    PreviewPane::Logs => &self.logs_editor,
                    PreviewPane::Notes => &self.notes_editor,
                };
                if editor.mode() == VimMode::Normal {
                    self.view = View::TaskList;
                    return Ok(false);
                }
                // Otherwise fall through to forward to editor
            }

            // Enter: Logs → attach tmux; Notes → start editing
            if key.code == KeyCode::Enter {
                match self.preview_pane {
                    PreviewPane::Logs => {
                        if let Some(task) = self.selected_task() {
                            if task.meta.is_multi_repo() && task.meta.repos.len() > 1 {
                                // Ensure all repo sessions exist before showing picker
                                for repo in &task.meta.repos {
                                    let _ = Tmux::ensure_session(
                                        &repo.tmux_session,
                                        &repo.worktree_path,
                                    );
                                }
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
                                let _ = Tmux::ensure_session(
                                    &task.meta.primary_repo().tmux_session,
                                    &task.meta.primary_repo().worktree_path,
                                );
                                if Tmux::session_exists(&task.meta.primary_repo().tmux_session) {
                                    return Ok(true);
                                }
                            } else if task.meta.is_multi_repo() {
                                let parent_session = Config::tmux_session_name(
                                    &task.meta.name,
                                    &task.meta.branch_name,
                                );
                                if let Some(ref parent_dir) = task.meta.parent_dir {
                                    if !Tmux::session_exists(&parent_session) {
                                        let _ = Tmux::create_session_with_windows(
                                            &parent_session,
                                            parent_dir,
                                        );
                                    }
                                }
                                if Tmux::session_exists(&parent_session) {
                                    return Ok(true);
                                }
                            }
                        }
                        return Ok(false);
                    }
                    PreviewPane::Notes => {
                        self.start_notes_editing();
                        return Ok(false);
                    }
                }
            }

            // Action keys — handled before forwarding to VimTextArea
            match key.code {
                KeyCode::Char('t') => {
                    self.open_task_editor();
                    return Ok(false);
                }
                KeyCode::Char('a') => {
                    // 'a' is "answer" when task is InputNeeded; otherwise forward to editor
                    if let Some(task) = self.selected_task() {
                        if task.meta.status == TaskStatus::InputNeeded {
                            self.open_task_editor();
                            return Ok(false);
                        }
                    }
                    // Fall through: for notes pane, 'a' triggers edit mode; for logs, blocked by read-only
                }
                KeyCode::Char('f') => {
                    self.start_feedback();
                    return Ok(false);
                }
                KeyCode::Char('x') => {
                    self.open_command_list();
                    return Ok(false);
                }
                KeyCode::Char('w') => {
                    self.open_queue();
                    return Ok(false);
                }
                KeyCode::Char('s') => {
                    self.stop_task()?;
                    return Ok(false);
                }
                KeyCode::Char('o') => {
                    let pr_info = self.selected_task().and_then(|t| {
                        t.meta
                            .linked_pr
                            .as_ref()
                            .map(|pr| (pr.number, pr.url.clone()))
                    });
                    if let Some((number, url)) = pr_info {
                        open_url(&url);
                        self.set_status(format!("Opening PR #{}...", number));
                    } else {
                        self.set_status("No linked PR".to_string());
                    }
                    return Ok(false);
                }
                KeyCode::Char('r') => {
                    self.start_restart_wizard()?;
                    return Ok(false);
                }
                KeyCode::Char('h') => {
                    self.toggle_hold()?;
                    return Ok(false);
                }
                _ => {}
            }

            // Insert-mode-entry keys in Notes pane start editing
            if self.preview_pane == PreviewPane::Notes {
                match key.code {
                    KeyCode::Char('i')
                    | KeyCode::Char('I')
                    | KeyCode::Char('o')
                    | KeyCode::Char('O') => {
                        self.start_notes_editing();
                        return Ok(false);
                    }
                    KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.start_notes_editing();
                        return Ok(false);
                    }
                    KeyCode::Char('A') => {
                        self.start_notes_editing();
                        return Ok(false);
                    }
                    _ => {}
                }
            }

            // Forward all remaining keys to the focused VimTextArea
            let input = Input::from(event.clone());
            match self.preview_pane {
                PreviewPane::Logs => self.logs_editor.input(input),
                PreviewPane::Notes => self.notes_editor.input(input),
            }
        }
        Ok(false)
    }

    fn start_notes_editing(&mut self) {
        self.notes_editing = true;
        self.notes_editor.set_read_only(false);
        self.notes_editor.set_insert_mode();
        self.set_status("Editing notes (vim mode, Ctrl+S or Esc twice to save)".to_string());
    }

    fn handle_notes_editing(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+S to save in any mode
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.notes_editing = false;
                self.notes_editor.set_normal_mode();
                self.notes_editor.set_read_only(true);
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
                self.notes_editor.set_read_only(true);
                self.save_notes()?;
                return Ok(false);
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

            self.task_file_editor = VimTextArea::from_lines(content.lines());
        }
        self.view = View::TaskEditor;
    }

    fn handle_task_editor_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+S to save and close in any mode
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.save_task_file()?;
                self.task_file_editor.set_normal_mode();

                // If the task is in InputNeeded state, resume the flow after saving
                if self
                    .selected_task()
                    .is_some_and(|t| t.meta.status == TaskStatus::InputNeeded)
                {
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
        }
        Ok(false)
    }

    fn handle_feedback_event(&mut self, event: Event) -> Result<bool> {
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
        }
        Ok(false)
    }

    fn handle_delete_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.archive_mode_index = (self.archive_mode_index + 1) % 3;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.archive_mode_index = (self.archive_mode_index + 2) % 3;
                }
                KeyCode::Enter => match self.archive_mode_index {
                    0 => self.archive_task(false)?,
                    1 => self.archive_task(true)?,
                    _ => self.fully_delete_task()?,
                },
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_respawn_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.respawn_confirm_index = (self.respawn_confirm_index + 1) % 2;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.respawn_confirm_index = if self.respawn_confirm_index == 0 {
                        1
                    } else {
                        0
                    };
                }
                KeyCode::Enter => {
                    if self.respawn_confirm_is_chief_of_staff {
                        match self.respawn_confirm_index {
                            0 => {
                                // Chief of Staff only
                                let target = "chief-of-staff".to_string();
                                tracing::info!(target = %target, "respawn confirmed: chief-of-staff only");
                                self.respawn_in_progress = Some(target.clone());
                                self.set_status("respawning chief-of-staff...".to_string());
                                let tx = self.respawn_tx.clone();
                                let config = self.config.clone();
                                self.rt.spawn(async move {
                                    let result = tokio::task::spawn_blocking(move || {
                                        use_cases::respawn_agent(
                                            &config,
                                            "chief-of-staff",
                                            false,
                                            120,
                                        )
                                    })
                                    .await
                                    .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
                                    let msg = match result {
                                        Ok(()) => Ok("chief-of-staff".to_string()),
                                        Err(e) => Err(format!("{e}")),
                                    };
                                    let _ = tx.send(msg);
                                });
                            }
                            1 => {
                                // Chief of Staff + all PMs
                                let project_names: Vec<String> =
                                    self.projects.iter().map(|p| p.meta.name.clone()).collect();
                                let pm_count = project_names.len();
                                tracing::info!(
                                    pm_count = pm_count,
                                    "respawn confirmed: cos + all pms"
                                );
                                self.respawn_in_progress = Some("cos+pms".to_string());
                                self.set_status(
                                    "respawning chief-of-staff + all pms...".to_string(),
                                );
                                let tx = self.respawn_tx.clone();
                                let config = self.config.clone();
                                self.rt.spawn(async move {
                                    // Respawn Chief of Staff first
                                    let cos_result = tokio::task::spawn_blocking({
                                        let config = config.clone();
                                        move || {
                                            use_cases::respawn_agent(
                                                &config,
                                                "chief-of-staff",
                                                false,
                                                120,
                                            )
                                        }
                                    })
                                    .await
                                    .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));

                                    if let Err(e) = cos_result {
                                        let _ = tx.send(Err(format!(
                                            "chief-of-staff respawn failed: {e}"
                                        )));
                                        return;
                                    }

                                    // Respawn all PMs concurrently
                                    let mut handles = Vec::new();
                                    for name in &project_names {
                                        let config = config.clone();
                                        let name = name.clone();
                                        handles.push(tokio::task::spawn_blocking(move || {
                                            use_cases::respawn_agent(&config, &name, false, 120)
                                        }));
                                    }

                                    let mut failures = Vec::new();
                                    for (i, handle) in handles.into_iter().enumerate() {
                                        match handle.await {
                                            Ok(Ok(())) => {}
                                            Ok(Err(e)) => {
                                                failures.push(format!("{}: {e}", project_names[i]))
                                            }
                                            Err(e) => {
                                                failures.push(format!("{}: {e}", project_names[i]))
                                            }
                                        }
                                    }

                                    if failures.is_empty() {
                                        let _ = tx.send(Ok(format!("cos+pms:{pm_count}")));
                                    } else {
                                        let _ = tx.send(Err(format!(
                                            "some respawns failed: {}",
                                            failures.join(", ")
                                        )));
                                    }
                                });
                            }
                            _ => {}
                        }
                    } else {
                        // PM confirmation
                        match self.respawn_confirm_index {
                            0 => {
                                // Respawn
                                if let Some(ref target) = self.respawn_confirm_target {
                                    let target = target.clone();
                                    tracing::info!(target = %target, "respawn confirmed: pm");
                                    self.respawn_in_progress = Some(target.clone());
                                    self.set_status(format!("respawning {target}..."));
                                    let tx = self.respawn_tx.clone();
                                    let config = self.config.clone();
                                    self.rt.spawn(async move {
                                        let result = tokio::task::spawn_blocking(move || {
                                            use_cases::respawn_agent(&config, &target, false, 120)
                                        })
                                        .await
                                        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
                                        let msg = match result {
                                            Ok(()) => Ok("pm".to_string()),
                                            Err(e) => Err(format!("{e}")),
                                        };
                                        let _ = tx.send(msg);
                                    });
                                }
                            }
                            1 => {
                                // Cancel — just return to previous view
                            }
                            _ => {}
                        }
                    }
                    self.view = self.respawn_confirm_return_view;
                    self.respawn_confirm_target = None;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = self.respawn_confirm_return_view;
                    self.respawn_confirm_target = None;
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
                                        || key.code == KeyCode::Up
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
                WizardStep::EnterDescription => {
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
                        self.command_list_state
                            .select(Some(self.selected_command_index));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !self.commands.is_empty() {
                        self.selected_command_index = if self.selected_command_index == 0 {
                            self.commands.len() - 1
                        } else {
                            self.selected_command_index - 1
                        };
                        self.command_list_state
                            .select(Some(self.selected_command_index));
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

    fn open_queue(&mut self) {
        if let Some(task) = self.selected_task() {
            if task.queued_item_count() == 0 {
                self.set_status("No items queued for this task".to_string());
                return;
            }
        } else {
            self.set_status("No task selected".to_string());
            return;
        }
        self.selected_queue_index = 0;
        self.view = View::Queue;
    }

    fn handle_queue_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            // Check for Ctrl+C to quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            let queue_len = self
                .selected_task()
                .map(|t| t.queued_item_count())
                .unwrap_or(0);

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::Preview;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if queue_len > 0 {
                        self.selected_queue_index = (self.selected_queue_index + 1) % queue_len;
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
                    self.delete_queue_item()?;
                }
                KeyCode::Char('c') => {
                    self.clear_queue()?;
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
                KeyCode::Esc => {
                    self.view = View::Preview;
                }
                KeyCode::Up | KeyCode::Down => {
                    let filtered = self.rebase_branch_filtered_indices();
                    if !filtered.is_empty() {
                        if key.code == KeyCode::Up {
                            self.selected_rebase_branch_index =
                                self.selected_rebase_branch_index.saturating_sub(1);
                        } else {
                            self.selected_rebase_branch_index =
                                (self.selected_rebase_branch_index + 1).min(filtered.len() - 1);
                        }
                    }
                }
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let filtered = self.rebase_branch_filtered_indices();
                    if !filtered.is_empty() {
                        self.selected_rebase_branch_index =
                            (self.selected_rebase_branch_index + 1).min(filtered.len() - 1);
                    }
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.selected_rebase_branch_index =
                        self.selected_rebase_branch_index.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let filtered = self.rebase_branch_filtered_indices();
                    if let Some(&real_idx) = filtered.get(self.selected_rebase_branch_index) {
                        if let Some(branch) = self.rebase_branches.get(real_idx).cloned() {
                            self.run_branch_command(&branch)?;
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.rebase_branch_search.input(Input {
                        key: Key::Backspace,
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                    self.selected_rebase_branch_index = 0;
                }
                KeyCode::Char(c) => {
                    self.rebase_branch_search.input(Input {
                        key: Key::Char(c),
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                    self.selected_rebase_branch_index = 0;
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
                    if let Some((_, session)) = self
                        .session_picker_sessions
                        .get(self.selected_session_index)
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
                        self.dismissed_notifs
                            .save(&self.config.dismissed_notifications_path());
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

    fn handle_show_prs_event(&mut self, event: Event) -> Result<bool> {
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
                    let total = self.show_prs_total_items();
                    if total > 0 && self.show_prs_selected < total - 1 {
                        self.show_prs_selected += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if self.show_prs_selected > 0 {
                        self.show_prs_selected -= 1;
                    }
                }
                KeyCode::Char('o') | KeyCode::Enter => {
                    if let Some(item) = self.show_prs_selected_item() {
                        let url = item.url.clone();
                        tracing::info!(url = %url, "opening GitHub item from show-prs");
                        open_url(&url);
                        self.set_status("Opening in browser...".to_string());
                    }
                }
                KeyCode::Char('r') => {
                    self.show_prs_poll_active = false; // force allow re-poll
                    self.start_show_prs_poll();
                    self.last_show_prs_poll = Instant::now();
                    self.set_status("Refreshing...".to_string());
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
                            let path = nv.current_dir.join(&entry.file_name);
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
                        let new_name = nv.rename_input.as_ref().unwrap().lines()[0]
                            .trim()
                            .to_string();
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
                NotesFocus::Explorer => match key.code {
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
                                let child_name = nv
                                    .current_dir
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string());
                                nv.current_dir = parent.to_path_buf();
                                let _ = nv.refresh();
                                nv.selected_index = child_name
                                    .and_then(|name| {
                                        nv.entries.iter().position(|e| e.file_name == name)
                                    })
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
                            match use_cases::move_note(
                                &dir,
                                &entry_name,
                                use_cases::MoveDirection::Down,
                            ) {
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
                            match use_cases::move_note(
                                &dir,
                                &entry_name,
                                use_cases::MoveDirection::Up,
                            ) {
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
                            self.view = View::ProjectList;
                        }
                    }
                    KeyCode::Char('q') => {
                        let _ = nv.save_current();
                        self.notes_view = None;
                        self.view = View::ProjectList;
                    }
                    _ => {}
                },
                NotesFocus::Editor => {
                    let vim_mode = nv.editor.mode();
                    let is_normal = vim_mode == VimMode::Normal;

                    if key.code == KeyCode::Tab && is_normal {
                        let _ = nv.save_current();
                        nv.focus = NotesFocus::Explorer;
                    } else if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('s')
                    {
                        let _ = nv.save_current();
                        self.set_status("Saved".to_string());
                    } else if key.code == KeyCode::Char('q') && is_normal {
                        let _ = nv.save_current();
                        self.notes_view = None;
                        self.view = View::ProjectList;
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
                ReviewWizardStep::EnterBranch => match key.code {
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
                    _ => match wizard.branch_source {
                        BranchSource::NewBranch => {
                            let input = Input::from(event.clone());
                            wizard.branch_editor.input(input);
                        }
                        BranchSource::ExistingBranch => match key.code {
                            KeyCode::Char('j') | KeyCode::Down => {
                                if !wizard.existing_branches.is_empty() {
                                    wizard.selected_branch_index = (wizard.selected_branch_index
                                        + 1)
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
                    },
                },
            }
        }
        Ok(false)
    }

    fn delete_queue_item(&mut self) -> Result<()> {
        if let Some(task) = self.tasks.get(self.selected_index) {
            let queue_len = task.queued_item_count();
            if queue_len == 0 {
                return Ok(());
            }

            use_cases::delete_queue_item(task, self.selected_queue_index)?;

            // Adjust selected index if needed
            let remaining = task.queued_item_count();
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

    fn clear_queue(&mut self) -> Result<()> {
        if let Some(task) = self.tasks.get_mut(self.selected_index) {
            use_cases::clear_queue(task)?;
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
        let (task_id, status, flow_name, tmux_session, task_content) = match self.selected_task() {
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
                    t.read_task()
                        .unwrap_or_else(|_| "No TASK.md available".to_string()),
                )
            }
            None => return Ok(()),
        };

        // If task is running, stop it first via the supervisor pathway. This
        // kills the live harness, finalizes the session, and restores any
        // pre-command flow snapshot — same as the `s` keybinding.
        if status == TaskStatus::Running {
            let config = self.config.clone();
            if let Some(task) = self.tasks.get_mut(self.selected_index) {
                if let Err(e) = use_cases::stop_task(&config, task) {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "failed to stop task before rerun"
                    );
                }
            }
            tracing::info!(task_id = %task_id, old_status = "running", new_status = "stopped", "stopped task before rerun");
            self.log_output(format!("Stopped {} before rerun", task_id));
        }
        // tmux_session is no longer used directly here — honor_stop handles
        // the agman pane. Keep the binding for compile-time clarity.
        let _ = &tmux_session;

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

        let task_editor = VimTextArea::from_lines(task_content.lines());

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
                            picker.selected_index = (picker.selected_index + 1) % total;
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
                    let should_select = self
                        .dir_picker
                        .as_ref()
                        .map(|p| {
                            p.is_repo_select_mode()
                                && (p.is_favorite_selected()
                                    || p.selected_entry_kind() == Some(DirKind::GitRepo))
                        })
                        .unwrap_or(false);

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
                            let is_fav = self
                                .dir_picker
                                .as_ref()
                                .map(|p| p.is_favorite_selected())
                                .unwrap_or(false);
                            let kind = self
                                .dir_picker
                                .as_ref()
                                .and_then(|p| p.selected_entry_kind());
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

                                let mut config_file =
                                    agman::config::load_config_file(&self.config.base_dir);
                                config_file.repos_dir =
                                    Some(selected_dir.to_string_lossy().to_string());
                                if let Err(e) = agman::config::save_config_file(
                                    &self.config.base_dir,
                                    &config_file,
                                ) {
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
                                    DirPickerOrigin::RepoSelect
                                    | DirPickerOrigin::ReviewRepoSelect => unreachable!(),
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
                    self.create_review_wizard_from_picker(entry_name, entry_path)?;
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

    // -----------------------------------------------------------------------
    // Inbox polling (Chief of Staff & PM message delivery)
    // -----------------------------------------------------------------------

    /// Targets whose consecutive-skip count has reached `STALL_THRESHOLD`.
    /// Surfaced in the TUI as `⚠ stalled` indicators.
    pub fn stalled_targets(&self) -> Vec<&str> {
        use_cases::stalled_targets_from_counts(&self.stuck_skip_counts, STALL_THRESHOLD)
    }

    fn start_inbox_poll(&mut self) {
        if self.inbox_poll_active {
            return;
        }

        // Enumerate delivery targets from disk so polling does not depend on
        // whichever TUI view the user has visited.
        let targets: Vec<_> =
            use_cases::collect_inbox_poll_targets(&self.config, Tmux::session_exists)
                .into_iter()
                .map(|t| {
                    (
                        t.name,
                        t.inbox_path,
                        t.seq_path,
                        t.session_name,
                        t.window,
                        t.rearm_path,
                    )
                })
                .collect();

        if targets.is_empty() {
            return;
        }

        self.inbox_poll_active = true;
        let tx = self.inbox_poll_tx.clone();
        let mut stuck_skip_counts = self.stuck_skip_counts.clone();
        let mut first_ready_at = self.first_ready_at.clone();

        self.rt.spawn(async move {
            let output = tokio::task::spawn_blocking(move || {
                const MAX_RETRIES: usize = 3;
                const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

                let mut results = Vec::new();
                for (target, inbox_path, seq_path, session_name, window, rearm_path) in targets {
                    let window_ref = window.as_deref();

                    // Cold-start re-arm: the supervisor touches `.inbox-rearm`
                    // across kill→relaunch transitions because the gap between
                    // the dying and freshly launched claude (~500ms) is shorter
                    // than the poll interval (~2s), so the poller almost never
                    // observes the brief shell state. Drop the stale
                    // `first_ready_at` entry and delete the marker so the next
                    // observed-ready tick restarts the 5s buffer from zero.
                    if let Some(path) = rearm_path.as_ref() {
                        if path.exists() {
                            first_ready_at.remove(&target);
                            if let Err(e) = std::fs::remove_file(path) {
                                tracing::debug!(
                                    target_name = &target,
                                    error = %e,
                                    "failed to remove .inbox-rearm marker"
                                );
                            }
                        }
                    }

                    let undelivered = match inbox::read_undelivered(&inbox_path, &seq_path) {
                        Ok(msgs) => msgs,
                        Err(e) => {
                            results.push(InboxPollResult {
                                target,
                                delivered: 0,
                                errors: vec![format!("read error: {e}")],
                            });
                            continue;
                        }
                    };

                    if undelivered.is_empty() {
                        stuck_skip_counts.remove(&target);
                        results.push(InboxPollResult {
                            target,
                            delivered: 0,
                            errors: vec![],
                        });
                        continue;
                    }

                    let mut delivered = 0;
                    let mut errors = Vec::new();

                    // Readiness gate (process-only check; no UI scraping) +
                    // 3-second cold-start buffer. Runs BEFORE the already-pasted
                    // rescue so a paste that landed during cold-start mounting
                    // still gets a chance to settle before we send Enter.
                    match Tmux::is_session_ready_in(&session_name, window_ref) {
                        Ok((false, cmd)) => {
                            tracing::info!(
                                target_name = &target,
                                session = &session_name,
                                cmd = %cmd,
                                "session not ready (foreground: {cmd}), skipping delivery this cycle"
                            );

                            // Re-arm the cold-start buffer: when claude restarts
                            // we want a fresh 3s window once it next flips ready.
                            first_ready_at.remove(&target);

                            let counter = stuck_skip_counts.entry(target.clone()).or_insert(0);
                            *counter += 1;

                            results.push(InboxPollResult {
                                target,
                                delivered: 0,
                                errors: vec![],
                            });
                            continue;
                        }
                        Err(e) => {
                            tracing::debug!(
                                target_name = &target,
                                session = &session_name,
                                error = %e,
                                "session readiness check failed, skipping"
                            );
                            results.push(InboxPollResult {
                                target,
                                delivered: 0,
                                errors: vec![],
                            });
                            continue;
                        }
                        Ok((true, _)) => {
                            let ready_since = *first_ready_at
                                .entry(target.clone())
                                .or_insert_with(Instant::now);
                            if ready_since.elapsed() < Duration::from_secs(5) {
                                results.push(InboxPollResult {
                                    target,
                                    delivered: 0,
                                    errors: vec![],
                                });
                                continue;
                            }
                            stuck_skip_counts.remove(&target);
                        }
                    }

                    // Decision 5: already-pasted rescue (after readiness+buffer)
                    let first_msg = &undelivered[0];
                    let first_snippet = format!("[msg:{}:{}]", first_msg.from, first_msg.seq);
                    let already_pasted = Tmux::capture_pane_window(&session_name, window_ref)
                        .map(|content| content.contains(&first_snippet))
                        .unwrap_or(false);

                    if already_pasted {
                        tracing::info!(
                            target_name = &target,
                            session = &session_name,
                            seq = first_msg.seq,
                            "already-pasted rescue: message visible in pane, sending Enter"
                        );
                        let _ = Tmux::send_enter_to(&session_name, window_ref);
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        let verified = Tmux::capture_pane_window(&session_name, window_ref)
                            .map(|content| content.contains(&first_snippet))
                            .unwrap_or(false);
                        if verified {
                            if let Err(e) = inbox::mark_delivered(&seq_path, first_msg.seq) {
                                errors.push(format!("mark_delivered error: {e}"));
                            } else {
                                delivered += 1;
                            }
                        }
                        // Even if rescue handled one message, skip to next target this cycle
                        // (subsequent messages will be delivered in future poll cycles)
                        stuck_skip_counts.remove(&target);
                        results.push(InboxPollResult {
                            target,
                            delivered,
                            errors,
                        });
                        continue;
                    }

                    'msg_loop: for msg in &undelivered {
                        let formatted_snippet = format!("[msg:{}:{}]", msg.from, msg.seq);

                        for attempt in 0..MAX_RETRIES {
                            let already_pasted = Tmux::capture_pane_window(&session_name, window_ref)
                                .map(|content| content.contains(&formatted_snippet))
                                .unwrap_or(false);

                            if already_pasted {
                                tracing::debug!(
                                    target = &target, seq = msg.seq, attempt = attempt,
                                    "message text already in pane, retrying Enter"
                                );
                                if let Err(e) = Tmux::send_enter_to(&session_name, window_ref) {
                                    tracing::warn!(
                                        target = &target, seq = msg.seq, error = %e,
                                        "failed to send Enter retry"
                                    );
                                    std::thread::sleep(RETRY_DELAY);
                                    continue;
                                }
                            } else if let Err(e) = Tmux::inject_message_to(&session_name, window_ref, &msg.from, &msg.message, msg.seq) {
                                tracing::warn!(
                                    target = &target, seq = msg.seq, attempt = attempt, error = %e,
                                    "inject_message failed"
                                );
                                std::thread::sleep(RETRY_DELAY);
                                continue;
                            }

                            std::thread::sleep(std::time::Duration::from_millis(200));
                            let verified = Tmux::capture_pane_window(&session_name, window_ref)
                                .map(|content| content.contains(&formatted_snippet))
                                .unwrap_or(false);

                            if verified {
                                if let Err(e) = inbox::mark_delivered(&seq_path, msg.seq) {
                                    errors.push(format!("mark_delivered error: {e}"));
                                } else {
                                    delivered += 1;
                                }
                                continue 'msg_loop;
                            }

                            tracing::debug!(
                                target = &target, seq = msg.seq, attempt = attempt,
                                "delivery verification failed, retrying"
                            );
                            std::thread::sleep(RETRY_DELAY);
                        }

                        errors.push(format!(
                            "delivery failed after {} attempts for seq {}", MAX_RETRIES, msg.seq
                        ));
                        break;
                    }
                    results.push(InboxPollResult {
                        target,
                        delivered,
                        errors,
                    });
                }
                InboxPollOutput {
                    results,
                    stuck_skip_counts,
                    first_ready_at,
                }
            })
            .await
            .unwrap_or_else(|_| InboxPollOutput {
                results: Vec::new(),
                stuck_skip_counts: std::collections::HashMap::new(),
                first_ready_at: std::collections::HashMap::new(),
            });
            let _ = tx.send(output);
        });
    }

    fn apply_inbox_poll_results(&mut self) {
        let output = match self.inbox_poll_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        self.inbox_poll_active = false;

        self.stuck_skip_counts = output.stuck_skip_counts;
        self.first_ready_at = output.first_ready_at;

        // One-shot warn the first time a target crosses STALL_THRESHOLD; clear
        // the warned marker when it recovers, so a later stall episode warns again.
        let currently_stalled: std::collections::HashSet<String> = self
            .stuck_skip_counts
            .iter()
            .filter(|(_, n)| **n >= STALL_THRESHOLD)
            .map(|(t, _)| t.clone())
            .collect();
        for target in &currently_stalled {
            if self.stall_warned.insert(target.clone()) {
                tracing::warn!(
                    target = %target,
                    threshold = STALL_THRESHOLD,
                    "message delivery stalled: session not ready for {} consecutive cycles",
                    STALL_THRESHOLD
                );
            }
        }
        self.stall_warned.retain(|t| currently_stalled.contains(t));

        for result in &output.results {
            if result.delivered > 0 {
                tracing::info!(
                    target = %result.target,
                    delivered = result.delivered,
                    "delivered inbox messages"
                );
                self.stuck_skip_counts.remove(&result.target);
            }
            for err in &result.errors {
                tracing::warn!(target = %result.target, error = %err, "inbox poll error");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Supervisor polling (drives flow progression for interactive-claude tasks)
    // -----------------------------------------------------------------------

    /// Spawn a background poll of every Running task's supervisor sentinel.
    /// For each task with a live session (`meta.session_history.last()`),
    /// call `supervisor::poll` and collect the outcome so the main loop can
    /// log it. Dispatch (advance flow, launch next agent, notify PM) is NOT
    /// wired yet — this iteration is observation-only.
    fn start_supervisor_poll(&mut self) {
        if self.supervisor_poll_active {
            return;
        }
        self.supervisor_poll_active = true;
        let config = self.config.clone();
        let tx = self.supervisor_poll_tx.clone();

        self.rt.spawn(async move {
            let output = tokio::task::spawn_blocking(move || {
                let mut items = Vec::new();
                for task in Task::list_all(&config) {
                    match supervisor::classify(&task) {
                        supervisor::PollTarget::Skip => continue,
                        supervisor::PollTarget::LiveSession { session_name } => {
                            match supervisor::poll(&task) {
                                Ok(outcome) => items.push(SupervisorPollItem::Tick {
                                    task_id: task.meta.task_id(),
                                    session_name,
                                    outcome,
                                }),
                                Err(e) => {
                                    tracing::warn!(
                                        task_id = %task.meta.task_id(),
                                        error = %e,
                                        "supervisor poll failed"
                                    );
                                }
                            }
                        }
                        supervisor::PollTarget::NeedsLaunch => {
                            items.push(SupervisorPollItem::NeedsLaunch {
                                task_id: task.meta.task_id(),
                            });
                        }
                    }
                }
                SupervisorPollOutput { items }
            })
            .await
            .unwrap_or_else(|_| SupervisorPollOutput { items: Vec::new() });
            let _ = tx.send(output);
        });
    }

    /// Drain any completed supervisor poll and dispatch outcomes.
    ///
    /// For `Condition(_)` this reloads the task from disk, calls
    /// `supervisor::advance` (which applies the condition, advances the flow
    /// step if appropriate, and launches the next agent), and logs the result.
    /// For `StopRequested` this honors the `.stop` sentinel by killing the
    /// running claude and transitioning the task to Stopped.
    fn apply_supervisor_poll_results(&mut self) {
        let output = match self.supervisor_poll_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        self.supervisor_poll_active = false;

        for item in output.items {
            match item {
                SupervisorPollItem::Tick {
                    task_id,
                    session_name,
                    outcome,
                } => match outcome {
                    supervisor::PollOutcome::Idle => {}
                    supervisor::PollOutcome::StopRequested => {
                        tracing::info!(
                            task_id = %task_id,
                            session_name = %session_name,
                            "supervisor poll: honoring stop sentinel"
                        );
                        let mut task = match Task::load_by_id(&self.config, &task_id) {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %task_id,
                                    error = %e,
                                    "failed to reload task for stop dispatch"
                                );
                                continue;
                            }
                        };
                        if let Err(e) = supervisor::honor_stop(&self.config, &mut task) {
                            tracing::error!(
                                task_id = %task_id,
                                error = %e,
                                "supervisor honor_stop failed"
                            );
                        }
                    }
                    supervisor::PollOutcome::Condition(cond) => {
                        tracing::info!(
                            task_id = %task_id,
                            session_name = %session_name,
                            condition = %cond,
                            "supervisor poll: dispatching condition"
                        );
                        let mut task = match Task::load_by_id(&self.config, &task_id) {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %task_id,
                                    error = %e,
                                    "failed to reload task for advance dispatch"
                                );
                                continue;
                            }
                        };
                        match supervisor::advance(&self.config, &mut task, cond) {
                            Ok(outcome) => {
                                tracing::info!(
                                    task_id = %task_id,
                                    outcome = ?outcome,
                                    "supervisor advance completed"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    task_id = %task_id,
                                    error = %e,
                                    "supervisor advance failed"
                                );
                            }
                        }
                    }
                },
                SupervisorPollItem::NeedsLaunch { task_id } => {
                    tracing::info!(
                        task_id = %task_id,
                        "supervisor poll: half-state detected, retrying launch_next_step"
                    );
                    let mut task = match Task::load_by_id(&self.config, &task_id) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task_id,
                                error = %e,
                                "failed to reload task for relaunch"
                            );
                            continue;
                        }
                    };
                    match supervisor::launch_next_step(&self.config, &mut task) {
                        Ok(outcome) => {
                            tracing::info!(
                                task_id = %task_id,
                                outcome = ?outcome,
                                "supervisor relaunch completed"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                task_id = %task_id,
                                error = %e,
                                "supervisor relaunch failed"
                            );
                        }
                    }
                }
            }
        }
    }

    fn apply_respawn_results(&mut self) {
        let result = match self.respawn_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        let target = self.respawn_in_progress.take().unwrap_or_default();
        match result {
            Ok(msg) if msg.starts_with("cos+pms:") => {
                let pm_count = msg.strip_prefix("cos+pms:").unwrap_or("0");
                tracing::info!(target = %target, pm_count = %pm_count, "cos + all pms respawned successfully");
                self.set_status(format!("respawned chief-of-staff + {pm_count} pms"));
            }
            Ok(_) => {
                tracing::info!(target = %target, "agent respawned successfully");
                self.set_status(format!("respawned {target}"));
            }
            Err(e) => {
                tracing::error!(target = %target, error = %e, "failed to respawn agent");
                self.set_status(format!("respawn failed: {e}"));
            }
        }
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

                        if let Err(e) = supervisor::ensure_task_tmux(task) {
                            self.log_output(format!(
                                "Failed to prepare tmux for {}: {}",
                                result.task_id, e
                            ));
                            continue;
                        }

                        match use_cases::queue_command(task, &self.config, "address-review", None) {
                            Ok(_) => {
                                let _ = use_cases::set_review_addressed(task, true);
                                self.log_output(format!(
                                    "Auto-triggered address-review for {}: new review on PR #{}",
                                    result.task_id, result.pr_number
                                ));
                            }
                            Err(e) => {
                                self.log_output(format!(
                                    "Failed to auto-trigger address-review for {}: {}",
                                    result.task_id, e
                                ));
                            }
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
                            let _ = use_cases::update_last_review_count(task, result.review_count);
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
                let mut task = self.tasks.remove(idx);

                // Kill all tmux sessions for the task
                for repo in &task.meta.repos {
                    let _ = Tmux::kill_session(&repo.tmux_session);
                }
                if task.meta.is_multi_repo() {
                    let parent_session =
                        Config::tmux_session_name(&task.meta.name, &task.meta.branch_name);
                    let _ = Tmux::kill_session(&parent_session);
                }

                let _ = use_cases::archive_task(&self.config, &mut task, false);

                self.log_output(format!(
                    "Auto-archived task {}: PR #{} merged",
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

        // Auto-dismiss CI/workflow failure notifications
        let ci_notifs: Vec<(String, String, String)> = self
            .notifications
            .iter()
            .filter(|n| n.reason == "ci_activity")
            .map(|n| (n.id.clone(), n.updated_at.clone(), n.title.clone()))
            .collect();
        if !ci_notifs.is_empty() {
            tracing::info!(
                count = ci_notifs.len(),
                titles = ?ci_notifs.iter().map(|(_, _, t)| t.as_str()).collect::<Vec<_>>(),
                "auto-dismissing ci_activity notifications"
            );
            for (thread_id, updated_at, _title) in &ci_notifs {
                self.dismissed_notifs
                    .insert(thread_id.clone(), updated_at.clone());
                let tid = thread_id.clone();
                self.rt.spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = use_cases::dismiss_github_notification(&tid) {
                            tracing::warn!(thread_id = %tid, error = %e, "failed to auto-dismiss ci_activity notification");
                        }
                    })
                    .await;
                });
            }
            self.dismissed_notifs
                .save(&self.config.dismissed_notifications_path());
        }

        // Filter out notifications that were dismissed but may not yet be reflected by the API.
        // Dismissed entries are only removed by retention-based pruning or explicit un-dismiss
        // when there's genuine new activity (unread + newer updated_at).
        if !self.dismissed_notifs.ids.is_empty() {
            // Prune entries older than the retention window
            let retention = chrono::Duration::weeks(
                agman::dismissed_notifications::NOTIFICATION_RETENTION_WEEKS,
            );
            let pruned = self.dismissed_notifs.prune_older_than(retention);

            // Un-dismiss threads that have new activity since they were dismissed
            let mut undismissed: Vec<(String, String, String)> = Vec::new();
            for notif in &self.notifications {
                if self.dismissed_notifs.should_undismiss(
                    &notif.id,
                    &notif.updated_at,
                    notif.unread,
                ) {
                    let old_updated_at = self
                        .dismissed_notifs
                        .ids
                        .get(&notif.id)
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

            if pruned > 0 || !undismissed.is_empty() {
                self.dismissed_notifs
                    .save(&self.config.dismissed_notifications_path());
                tracing::debug!(
                    pruned,
                    undismissed = undismissed.len(),
                    "cleaned up dismissed notification entries"
                );
            }
            // Filter out still-dismissed notifications from the displayed list
            let before = self.notifications.len();
            self.notifications
                .retain(|n| !self.dismissed_notifs.contains(&n.id));
            let filtered = before - self.notifications.len();
            if filtered > 0 {
                tracing::debug!(
                    filtered_count = filtered,
                    "filtered dismissed notifications from poll results"
                );
            }
        }

        // Clamp selection index
        if self.selected_notif_index >= self.notifications.len() && !self.notifications.is_empty() {
            self.selected_notif_index = self.notifications.len() - 1;
        }

        tracing::debug!(
            notification_count = self.notifications.len(),
            "applied github notification poll results"
        );
    }

    /// Spawn a background task to poll GitHub issues & PRs for the Show PRs view.
    fn start_show_prs_poll(&mut self) {
        if self.show_prs_poll_active {
            return;
        }

        self.show_prs_poll_active = true;
        let tx = self.show_prs_poll_tx.clone();

        tracing::debug!("starting show-prs poll");
        self.rt.spawn(async move {
            let result = tokio::task::spawn_blocking(use_cases::fetch_show_prs_data)
                .await
                .unwrap_or_else(|_| use_cases::ShowPrsData::default());
            let _ = tx.send(result);
        });
    }

    /// Check for completed Show PRs poll results (non-blocking) and apply.
    fn apply_show_prs_results(&mut self) {
        let result = match self.show_prs_poll_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        self.show_prs_poll_active = false;

        if !self.show_prs_first_poll_done {
            self.show_prs_first_poll_done = true;
            tracing::debug!("first show-prs poll completed");
        }

        self.show_prs_data = result;

        // Clamp selection index
        let total = self.show_prs_total_items();
        if self.show_prs_selected >= total && total > 0 {
            self.show_prs_selected = total - 1;
        }

        tracing::debug!(
            issues = self.show_prs_data.issues.len(),
            my_prs = self.show_prs_data.my_prs.len(),
            review_requests = self.show_prs_data.review_requests.len(),
            "applied show-prs poll results"
        );
    }

    fn show_prs_total_items(&self) -> usize {
        self.show_prs_data.issues.len()
            + self.show_prs_data.my_prs.len()
            + self.show_prs_data.review_requests.len()
    }

    fn show_prs_selected_item(&self) -> Option<&use_cases::GithubItem> {
        let idx = self.show_prs_selected;
        let issues_len = self.show_prs_data.issues.len();
        let my_prs_len = self.show_prs_data.my_prs.len();

        if idx < issues_len {
            self.show_prs_data.issues.get(idx)
        } else if idx < issues_len + my_prs_len {
            self.show_prs_data.my_prs.get(idx - issues_len)
        } else {
            self.show_prs_data
                .review_requests
                .get(idx - issues_len - my_prs_len)
        }
    }

    /// Spawn a background task to poll Keybase unread conversations.
    fn start_keybase_poll(&mut self) {
        if self.keybase_poll_active || !self.keybase_available {
            return;
        }

        self.keybase_poll_active = true;
        let tx = self.keybase_tx.clone();

        tracing::debug!("starting keybase unread poll");
        self.rt.spawn(async move {
            let result = tokio::task::spawn_blocking(use_cases::fetch_keybase_unreads)
                .await
                .unwrap_or(use_cases::KeybasePollResult {
                    dm_unread_count: 0,
                    channel_unread_count: 0,
                    keybase_available: true,
                });
            let _ = tx.send(result);
        });
    }

    /// Check for completed Keybase poll results (non-blocking) and apply.
    fn apply_keybase_poll_results(&mut self) {
        let result = match self.keybase_rx.try_recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        self.keybase_poll_active = false;

        if !self.keybase_first_poll_done {
            self.keybase_first_poll_done = true;
            tracing::debug!("first keybase unread poll completed");
        }

        if !result.keybase_available {
            self.keybase_available = false;
            tracing::warn!("keybase not available, disabling poll");
        }

        self.keybase_dm_unread_count = result.dm_unread_count;
        self.keybase_channel_unread_count = result.channel_unread_count;
        tracing::debug!(
            unread_dm = result.dm_unread_count,
            unread_channel = result.channel_unread_count,
            "applied keybase poll results"
        );
    }

    fn execute_restart_wizard(&mut self) -> Result<()> {
        let (task_id, selected_step_index) = match &self.restart_wizard {
            Some(w) => (w.task_id.clone(), w.selected_step_index),
            None => return Ok(()),
        };

        tracing::info!(task_id = %task_id, step = selected_step_index, "TUI: rerun task from wizard");
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
        if !task_meta.has_repos() && !task_meta.is_multi_repo() {
            self.set_status(format!("Task {} has no repos configured", task_id));
            self.restart_wizard = None;
            self.view = View::TaskList;
            return Ok(());
        }

        // Set flow_step and status, then launch via supervisor.
        use_cases::restart_task(&mut self.tasks[task_idx], selected_step_index)?;
        let _ = use_cases::set_review_addressed(&mut self.tasks[task_idx], false);

        let task = &mut self.tasks[task_idx];
        let launch_error = supervisor::ensure_task_tmux(task)
            .and_then(|_| supervisor::launch_next_step(&self.config, task).map(|_| ()))
            .err();

        match launch_error {
            None => {
                self.set_status(format!(
                    "Rerun: {} from step {}",
                    task_id, selected_step_index
                ));
            }
            Some(e) => {
                tracing::error!(task_id = %task_id, error = %e, "failed to relaunch via supervisor");
                self.set_status(format!("Rerun failed: {}", e));
            }
        }

        self.restart_wizard = None;
        self.view = View::TaskList;
        self.refresh_tasks_and_select(&task_id);

        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_caffeinate();
    }
}

/// Score a branch name against search terms for relevance ranking.
/// Higher score = more relevant. Used by `rebase_branch_filtered_indices()`.
fn branch_search_score(branch: &str, terms: &[&str]) -> i64 {
    let branch_lower = branch.to_lowercase();
    let segments: Vec<&str> = branch_lower.split(['/', '-']).collect();
    let mut score: i64 = 0;
    for term in terms {
        if branch_lower == *term {
            score += 1000;
        } else if segments.iter().any(|seg| seg.starts_with(term)) {
            score += 100;
        }
    }
    // Shorter branch names rank higher as tiebreaker
    score -= branch.len() as i64;
    score
}

pub fn run_tui(config: Config) -> Result<()> {
    // Remove any stale restart signal files left over from a previous run.
    // This prevents a "double restart" if the TUI missed the signal (e.g. it
    // crashed or was not running when release.sh created the file).
    #[cfg(unix)]
    {
        let base = dirs::home_dir().unwrap_or_default().join(".agman");
        let name = ".agman-restart";
        let path = base.join(name);
        if path.exists() {
            tracing::info!(file = name, "removing stale restart signal file at startup");
            let _ = std::fs::remove_file(&path);
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
        app.current_project = None;
        app.view = View::ProjectList;
        app.should_quit = false;
        app.refresh_projects();
        app.refresh_tasks();

        // Main loop
        let mut attach_session: Option<String> = None;
        let mut last_refresh = Instant::now();
        let refresh_interval = Duration::from_secs(3);

        loop {
            // Poll any active tmux popup so inbox and PR polling keep ticking
            // while the user is interacting with the popup. When the popup
            // process exits, dispatch close side-effects via `on_popup_closed`.
            let popup_open = match app.popup.as_mut() {
                Some(p) => match p.child.try_wait() {
                    Ok(None) => true,
                    Ok(Some(_)) => {
                        app.on_popup_closed();
                        false
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "popup try_wait failed, treating as closed");
                        app.on_popup_closed();
                        false
                    }
                },
                None => false,
            };

            if !popup_open {
                terminal.draw(|f| ui::draw(f, &mut app))?;
            }

            if event::poll(Duration::from_millis(250))? {
                let event = event::read()?;
                if !popup_open {
                    let should_attach = app.handle_event(event)?;

                    if should_attach {
                        // Session picker sets attach_session_name directly
                        if let Some(session) = app.attach_session_name.take() {
                            attach_session = Some(session);
                        } else if let Some(task) = app.selected_task() {
                            if task.meta.has_repos() {
                                attach_session =
                                    Some(task.meta.primary_repo().tmux_session.clone());
                            } else if task.meta.is_multi_repo() {
                                // Multi-repo with no repos yet — attach to parent session
                                attach_session = Some(Config::tmux_session_name(
                                    &task.meta.name,
                                    &task.meta.branch_name,
                                ));
                            }
                        }
                        break;
                    }
                }
                // While a popup is open, tmux owns input. Any crossterm events
                // that leak through (e.g. SIGWINCH resize) target stale pane
                // geometry and must be discarded rather than dispatched.
            }

            if app.should_quit || app.should_restart {
                break;
            }

            // Periodic refresh (every 3 seconds)
            if last_refresh.elapsed() >= refresh_interval {
                if app.view == View::ProjectList {
                    app.refresh_projects();
                } else if app.view == View::ResearcherList {
                    app.refresh_researchers();
                } else if app.view == View::TaskList {
                    app.refresh_tasks_for_project();
                    // Check for stranded queue items on stopped tasks
                    app.process_stranded_queue();
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

            // Poll Show PRs data every 60 seconds (regardless of view)
            if app.last_show_prs_poll.elapsed() >= Duration::from_secs(60) {
                app.start_show_prs_poll();
                app.last_show_prs_poll = Instant::now();
            }

            // Check for completed Show PRs poll results (non-blocking)
            app.apply_show_prs_results();

            // Poll Keybase unreads every 2 seconds (local Unix socket, ~49ms per call)
            if app.last_keybase_poll.elapsed() >= Duration::from_secs(2) {
                app.start_keybase_poll();
                app.last_keybase_poll = Instant::now();
            }

            // Check for completed Keybase poll results (non-blocking)
            app.apply_keybase_poll_results();

            // Poll agent inboxes every 2 seconds (deliver messages via tmux send-keys)
            if app.last_inbox_poll.elapsed() >= Duration::from_secs(2) {
                app.start_inbox_poll();
                app.last_inbox_poll = Instant::now();
            }

            // Check for completed inbox poll results (non-blocking)
            app.apply_inbox_poll_results();

            // Poll task supervisor sentinels every 3 seconds. The supervisor
            // drives flow progression — `apply_supervisor_poll_results` reacts
            // to detected stop conditions and calls into `supervisor::advance`.
            if app.last_supervisor_poll.elapsed() >= Duration::from_secs(3) {
                app.start_supervisor_poll();
                app.last_supervisor_poll = Instant::now();
            }
            app.apply_supervisor_poll_results();

            // Check for completed respawn results (non-blocking)
            app.apply_respawn_results();

            if app.last_telegram_watchdog.elapsed() >= TELEGRAM_WATCHDOG_INTERVAL {
                app.check_telegram_watchdog();
                app.last_telegram_watchdog = Instant::now();
            }

            // Check for restart signal (written by release.sh or `agman restart`)
            #[cfg(unix)]
            {
                let restart_signal = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".agman/.agman-restart");
                if restart_signal.exists() {
                    tracing::info!("detected .agman-restart signal file, restarting immediately");
                    let _ = std::fs::remove_file(&restart_signal);
                    app.should_restart = true;
                    break;
                }
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

        // Signal Telegram bot to stop
        if let Some(ref h) = app.telegram {
            h.cancel.store(true, Ordering::Relaxed);
        }

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
