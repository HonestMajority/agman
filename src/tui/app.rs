use anyhow::Result;
use ratatui::crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, widgets::ListState, Terminal};
use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc as tokio_mpsc;
use tui_textarea::{CursorMove, Input, Key, TextArea};

use agman::agent_model::{AgentAttachment, AgentKind, AgentRecord, AgentStatus};
use agman::config::Config;
use agman::dismissed_notifications::DismissedNotifications;
use agman::git::Git;
use agman::inbox;
use agman::project::Project;
use agman::repo_stats::RepoStats;
use agman::supervisor;
use agman::task::Task;
use agman::tmux::{Tmux, TmuxWindowActivity};
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
    NewTaskWizard,
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
    AgentWizard,
    RespawnConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Tasks,
    Agents,
}

#[derive(Debug, Clone, Copy)]
pub enum ProjectTaskRow<'a> {
    Task {
        task_index: usize,
        task: &'a Task,
    },
    Agent {
        task_index: usize,
        agent: &'a AgentRecord,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectTaskRowKey {
    Task {
        task_id: String,
    },
    Agent {
        task_id: String,
        project: String,
        name: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum ProjectDetailRow<'a> {
    AgentsSectionHeader,
    SectionColumnSpacer,
    AgentsColumnsHeader,
    EmptyAgents,
    UnattachedAgent { agent: &'a AgentRecord },
    SectionSpacer,
    TaskGroupSpacer,
    TasksSectionHeader,
    TasksColumnsHeader,
    EmptyTasks,
    Task(ProjectTaskRow<'a>),
    AttachedAgentsHeader,
    AttachedAgent(ProjectTaskRow<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectDetailRowKey {
    UnattachedAgent { project: String, name: String },
    Task(ProjectTaskRowKey),
    AttachedAgent(ProjectTaskRowKey),
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
    /// Repo selection: browse directories to choose a repo or multi-repo parent.
    RepoSelect,
}

/// Re-export DirKind for use in the picker UI.
pub use agman::use_cases::DirKind;

pub struct DirectoryPicker {
    pub current_dir: PathBuf,
    pub entries: Vec<String>,
    /// Classification of each entry (parallel to `entries`). Only populated for `RepoSelect` origin.
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
        matches!(self.origin, DirPickerOrigin::RepoSelect)
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
    /// True when a multi-repo parent directory was selected (not a git repo).
    pub is_multi_repo: bool,
}

impl NewTaskWizard {
    /// The selected repo name.
    pub fn selected_repo_name(&self) -> &str {
        &self.selected_repo
    }
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

/// Steps in the create-agent wizard.
///
/// Researchers go directly from `Name` → `Description` (matching the legacy
/// agent). Worktree-backed agents insert a `Worktrees` step; testers add
/// an optional capabilities step before description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWizardStep {
    /// Step 0: pick the kind (Researcher | Reviewer | Tester | Operator).
    Kind,
    /// Step 1: enter the agent name.
    Name,
    /// Step 2 (Reviewer/Tester only): edit the (repo, branch) row list.
    Worktrees,
    /// Step 3 (Tester only): optional capability toggles.
    Capabilities,
    /// Final step: enter the description.
    Description,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWizardKind {
    Researcher,
    Reviewer,
    Tester,
    Operator,
}

/// One editable `(repo, branch)` row in the reviewer wizard.
pub struct ReviewerWorktreeRow {
    pub repo_editor: TextArea<'static>,
    pub branch_editor: TextArea<'static>,
    /// false = repo focused, true = branch focused
    pub branch_focus: bool,
}

impl ReviewerWorktreeRow {
    pub fn new() -> Self {
        let mut repo_editor = TextArea::default();
        repo_editor.set_cursor_line_style(ratatui::style::Style::default());
        let mut branch_editor = TextArea::default();
        branch_editor.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            repo_editor,
            branch_editor,
            branch_focus: false,
        }
    }

    pub fn repo(&self) -> String {
        self.repo_editor.lines().join("").trim().to_string()
    }

    pub fn branch(&self) -> String {
        self.branch_editor.lines().join("").trim().to_string()
    }
}

pub struct AgentWizard {
    pub kind: AgentWizardKind,
    pub step: AgentWizardStep,
    pub name_editor: TextArea<'static>,
    pub description_editor: VimTextArea<'static>,
    /// Reviewer/Tester-only: editable `(repo, branch)` rows. The `selected_row` is
    /// the row currently focused for j/k navigation in the worktrees step.
    pub worktree_rows: Vec<ReviewerWorktreeRow>,
    pub selected_row: usize,
    pub browser_capability: bool,
    pub error_message: Option<String>,
    pub project: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPane {
    Logs,
    Notes,
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

struct ProjectRefreshSnapshot {
    projects: Vec<Project>,
    project_task_counts: HashMap<String, usize>,
    project_agent_counts: HashMap<String, usize>,
    project_active_agent_counts: HashMap<String, usize>,
    unassigned_task_count: usize,
    agent_activity: HashMap<String, AgentActivitySample>,
    project_list_error: Option<String>,
    agent_list_error: Option<String>,
    agent_activity_error: Option<String>,
}

impl ProjectRefreshSnapshot {
    fn worker_failed(error: String) -> Self {
        Self {
            projects: Vec::new(),
            project_task_counts: HashMap::new(),
            project_agent_counts: HashMap::new(),
            project_active_agent_counts: HashMap::new(),
            unassigned_task_count: 0,
            agent_activity: HashMap::new(),
            project_list_error: Some(error),
            agent_list_error: None,
            agent_activity_error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentActivitySample {
    pub last_tmux_activity_epoch: Option<i64>,
    pub last_observed_work_at: Option<Instant>,
    pub foreground_command: String,
    pub pane_dead: bool,
    pub query_ok: bool,
}

impl AgentActivitySample {
    fn from_tmux_window(
        activity: &TmuxWindowActivity,
        observed_at: Instant,
        now_epoch_secs: Option<i64>,
    ) -> Self {
        let last_observed_work_at = match (activity.window_activity, now_epoch_secs) {
            (Some(activity_epoch), Some(now_epoch)) => {
                let age_secs = now_epoch.saturating_sub(activity_epoch).max(0) as u64;
                observed_at.checked_sub(Duration::from_secs(age_secs))
            }
            _ => None,
        };

        Self {
            last_tmux_activity_epoch: activity.window_activity,
            last_observed_work_at,
            foreground_command: activity.pane_current_command.clone(),
            pane_dead: activity.pane_dead,
            query_ok: true,
        }
    }

    pub fn foreground_command_is_shell(&self) -> bool {
        Tmux::is_shell_command(&self.foreground_command)
    }

    pub fn activity_age(&self, now: Instant) -> Duration {
        if self.last_tmux_activity_epoch.is_none() {
            return Duration::MAX;
        }
        self.last_observed_work_at
            .map(|at| now.saturating_duration_since(at))
            .unwrap_or(Duration::MAX)
    }
}

/// How many consecutive "skipped" poll cycles (readiness gate refused to
/// deliver while the inbox still had undelivered messages) qualifies a target
/// as "stalled" and surfaces a UI indicator. At the 2s poll cadence this is
/// ~10 seconds.
pub const STALL_THRESHOLD: u32 = 5;

fn unix_epoch_secs() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

fn build_agent_activity_snapshot(
    active_sessions: &HashSet<String>,
) -> (HashMap<String, AgentActivitySample>, Option<String>) {
    if active_sessions.is_empty() {
        return (HashMap::new(), None);
    }

    let windows = match Tmux::list_window_activity() {
        Ok(windows) => windows,
        Err(e) => return (HashMap::new(), Some(e.to_string())),
    };

    let mut by_session: HashMap<String, TmuxWindowActivity> = HashMap::new();
    for activity in windows {
        if !active_sessions.contains(&activity.session_name) {
            continue;
        }
        let replace = by_session
            .get(&activity.session_name)
            .map(|current| activity.window_activity > current.window_activity)
            .unwrap_or(true);
        if replace {
            by_session.insert(activity.session_name.clone(), activity);
        }
    }

    let observed_at = Instant::now();
    let now_epoch_secs = unix_epoch_secs();
    let mut agent_activity = HashMap::new();
    for session in active_sessions {
        if let Some(activity) = by_session.get(session) {
            agent_activity.insert(
                session.clone(),
                AgentActivitySample::from_tmux_window(activity, observed_at, now_epoch_secs),
            );
        }
    }

    (agent_activity, None)
}

fn build_project_refresh_snapshot(config: Config) -> ProjectRefreshSnapshot {
    let mut project_list_error = None;
    let mut projects = use_cases::list_projects(&config).unwrap_or_else(|e| {
        project_list_error = Some(e.to_string());
        Vec::new()
    });
    // Sort held projects to the bottom (stable sort preserves alphabetical order within groups)
    projects.sort_by_key(|p| p.meta.held);

    let mut project_task_counts = HashMap::new();
    let mut unassigned_task_count = 0;
    for task in Task::list_all(&config) {
        if let Some(proj) = task.meta.project {
            *project_task_counts.entry(proj).or_insert(0) += 1;
        } else {
            unassigned_task_count += 1;
        }
    }

    let mut project_agent_counts = HashMap::new();
    let mut project_active_agent_counts = HashMap::new();
    let mut active_sessions = HashSet::new();
    let mut project_agents = Vec::new();
    let mut agent_list_error = None;

    match use_cases::list_agents(&config, None, None) {
        Ok(agents) => {
            for agent in agents {
                if agent.meta.project == "chief-of-staff"
                    || agent.meta.status == AgentStatus::Archived
                {
                    continue;
                }
                active_sessions.insert(App::agent_session_name(&agent));
                *project_agent_counts
                    .entry(agent.meta.project.clone())
                    .or_insert(0) += 1;
                project_agents.push(agent);
            }
        }
        Err(e) => {
            agent_list_error = Some(e.to_string());
        }
    }

    let (agent_activity, agent_activity_error) = build_agent_activity_snapshot(&active_sessions);
    let now = Instant::now();
    for agent in project_agents {
        let session_name = App::agent_session_name(&agent);
        if ui::classify_agent_status(now, agent_activity.get(&session_name))
            == ui::WorkingIdle::Working
        {
            *project_active_agent_counts
                .entry(agent.meta.project)
                .or_insert(0) += 1;
        }
    }

    ProjectRefreshSnapshot {
        projects,
        project_task_counts,
        project_agent_counts,
        project_active_agent_counts,
        unassigned_task_count,
        agent_activity,
        project_list_error,
        agent_list_error,
        agent_activity_error,
    }
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
    pub should_quit: bool,
    pub status_message: Option<(String, Instant)>,
    pub wizard: Option<NewTaskWizard>,
    pub output_log: Vec<String>,
    pub output_scroll: u16,
    pub last_output_time: Option<Instant>,
    pub should_restart: bool,
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
    // Notes view
    pub notes_view: Option<NotesView>,
    pub notes_return_view: View,
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
    pub archive_kind: ArchiveKind,
    pub archive_tasks: Vec<(Task, String)>,
    pub archive_agents: Vec<(AgentRecord, String)>,
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
    pub project_task_counts: std::collections::HashMap<String, usize>,
    pub project_agent_counts: std::collections::HashMap<String, usize>,
    pub project_active_agent_counts: std::collections::HashMap<String, usize>,
    project_refresh_tx: tokio_mpsc::UnboundedSender<(u64, ProjectRefreshSnapshot)>,
    project_refresh_rx: tokio_mpsc::UnboundedReceiver<(u64, ProjectRefreshSnapshot)>,
    project_refresh_active: bool,
    project_refresh_generation: u64,
    // Project wizard
    pub project_wizard: Option<ProjectWizard>,
    pub agent_wizard: Option<AgentWizard>,
    // Project picker (for task migration/move)
    pub project_picker: Option<ProjectPicker>,
    // Project deletion
    pub project_to_delete: Option<String>,
    // Unattached project agents plus task-attached child rows.
    pub agents: Vec<AgentRecord>,
    pub attached_task_agents: HashMap<String, Vec<AgentRecord>>,
    pub agent_activity: HashMap<String, AgentActivitySample>,
    agent_activity_query_failed_logged: bool,
    // Inbox polling
    pub last_inbox_poll: Instant,
    inbox_poll_tx: tokio_mpsc::UnboundedSender<InboxPollOutput>,
    inbox_poll_rx: tokio_mpsc::UnboundedReceiver<InboxPollOutput>,
    inbox_poll_active: bool,
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
    // Task archive (async). `archive_in_progress` holds task ids that the
    // worker is currently archiving — the live task list filter consults this
    // set so a row removed synchronously doesn't flicker back from disk
    // between the optimistic remove and the worker rewriting `archived_at`.
    pub archive_in_progress: HashSet<String>,
    archive_tx: tokio_mpsc::UnboundedSender<(String, Result<bool, String>)>,
    archive_rx: tokio_mpsc::UnboundedReceiver<(String, Result<bool, String>)>,
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
    // Active tmux popup (CoS/PM chat, agent attach). Polled each main-loop
    // tick so inbox delivery and PR polls keep running while a
    // popup is open.
    popup: Option<ActivePopup>,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        Self::new_with_options(config, true)
    }

    #[cfg(test)]
    fn new_for_test(config: Config) -> Result<Self> {
        Self::new_with_options(config, false)
    }

    fn new_with_options(config: Config, autostart_sessions: bool) -> Result<Self> {
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
        let notes_editor = VimTextArea::new();
        let mut logs_editor = VimTextArea::new();
        logs_editor.set_read_only(true);
        let (gh_notif_tx, gh_notif_rx) = tokio_mpsc::unbounded_channel();
        let (show_prs_poll_tx, show_prs_poll_rx) = tokio_mpsc::unbounded_channel();
        let (inbox_poll_tx, inbox_poll_rx) = tokio_mpsc::unbounded_channel();
        let (project_refresh_tx, project_refresh_rx) = tokio_mpsc::unbounded_channel();
        let (respawn_tx, respawn_rx) = tokio_mpsc::unbounded_channel();
        let (archive_tx, archive_rx) = tokio_mpsc::unbounded_channel();
        let rt = tokio::runtime::Runtime::new()?;
        let mut dismissed_notifs =
            DismissedNotifications::load(&config.dismissed_notifications_path());
        let retention =
            chrono::Duration::weeks(agman::dismissed_notifications::NOTIFICATION_RETENTION_WEEKS);
        if dismissed_notifs.prune_older_than(retention) > 0 {
            dismissed_notifs.save(&config.dismissed_notifications_path());
        }

        if autostart_sessions {
            // Auto-start the Chief of Staff agent session in the background
            if let Err(e) = use_cases::start_chief_of_staff_session(&config, false) {
                tracing::error!(error = %e, "failed to auto-start Chief of Staff session on launch");
            }

            // Auto-start PM sessions for all projects
            if let Ok(projects) = use_cases::list_projects(&config) {
                for project in &projects {
                    if let Err(e) = use_cases::start_pm_session(&config, &project.meta.name, false)
                    {
                        tracing::error!(project = %project.meta.name, error = %e, "failed to auto-start PM session on launch");
                    }
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
            should_quit: false,
            status_message: None,
            wizard: None,
            output_log: Vec::new(),
            output_scroll: 0,
            last_output_time: None,
            should_restart: false,
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
            notes_view: None,
            notes_return_view: View::ProjectList,
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
            archive_kind: ArchiveKind::Tasks,
            archive_tasks: Vec::new(),
            archive_agents: Vec::new(),
            archive_search: Self::create_plain_editor(),
            archive_selected: 0,
            archive_list_state: ListState::default(),
            archive_preview: None,
            archive_scroll: 0,
            projects: Vec::new(),
            selected_project_index: 0,
            current_project: None,
            unassigned_task_count: 0,
            project_task_counts: std::collections::HashMap::new(),
            project_agent_counts: std::collections::HashMap::new(),
            project_active_agent_counts: std::collections::HashMap::new(),
            project_refresh_tx,
            project_refresh_rx,
            project_refresh_active: false,
            project_refresh_generation: 0,
            project_wizard: None,
            agent_wizard: None,
            project_picker: None,
            project_to_delete: None,
            agents: Vec::new(),
            attached_task_agents: HashMap::new(),
            agent_activity: HashMap::new(),
            agent_activity_query_failed_logged: false,
            last_inbox_poll: Instant::now(),
            inbox_poll_tx,
            inbox_poll_rx,
            inbox_poll_active: false,
            stuck_skip_counts: std::collections::HashMap::new(),
            first_ready_at: std::collections::HashMap::new(),
            stall_warned: std::collections::HashSet::new(),
            respawn_in_progress: None,
            respawn_tx,
            respawn_rx,
            archive_in_progress: HashSet::new(),
            archive_tx,
            archive_rx,
            respawn_confirm_target: None,
            respawn_confirm_index: 0,
            respawn_confirm_is_chief_of_staff: false,
            respawn_confirm_return_view: View::ProjectList,
            telegram,
            last_telegram_watchdog: Instant::now(),
            last_telegram_respawn_at: None,
            #[cfg(target_os = "macos")]
            caffeinate_process: if autostart_sessions {
                std::process::Command::new("caffeinate")
                    .arg("-s")
                    .spawn()
                    .ok()
            } else {
                None
            },
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
        let prev_row_key = self.selected_project_detail_row_key();
        self.tasks = Task::list_all(&self.config);
        self.refresh_attached_task_agents();
        if self.restore_project_detail_selection(prev_row_key.as_ref()) {
            return;
        }
        self.clamp_project_detail_selection();
    }

    /// Refresh the task list and restore selection to the task with the given ID.
    /// If the task is no longer present, selection falls back to a valid index.
    fn refresh_tasks_and_select(&mut self, task_id: &str) {
        self.refresh_tasks_for_project();
        if let Some(idx) = self.project_detail_rows().iter().position(|row| {
            matches!(
                row,
                ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. })
                    if task.meta.task_id() == task_id
            )
        }) {
            self.selected_index = idx;
        }
    }

    pub fn refresh_projects(&mut self) {
        self.project_refresh_generation = self.project_refresh_generation.wrapping_add(1);
        let snapshot = build_project_refresh_snapshot(self.config.clone());
        self.apply_project_refresh_snapshot(snapshot);
    }

    fn apply_project_refresh_snapshot(&mut self, snapshot: ProjectRefreshSnapshot) {
        if let Some(error) = snapshot.project_list_error {
            tracing::warn!(error = %error, "failed to list projects");
        }
        if let Some(error) = snapshot.agent_list_error {
            tracing::warn!(error = %error, "failed to count project agents");
        }
        if let Some(error) = snapshot.agent_activity_error {
            if !self.agent_activity_query_failed_logged {
                tracing::warn!(
                    error = %error,
                    "failed to query tmux activity; agent statuses will be idle"
                );
                self.agent_activity_query_failed_logged = true;
            }
        } else {
            self.agent_activity_query_failed_logged = false;
        }

        self.projects = snapshot.projects;
        self.project_task_counts = snapshot.project_task_counts;
        self.project_agent_counts = snapshot.project_agent_counts;
        self.project_active_agent_counts = snapshot.project_active_agent_counts;
        self.unassigned_task_count = snapshot.unassigned_task_count;
        self.agent_activity = snapshot.agent_activity;

        let total = self.project_list_len();
        if self.selected_project_index >= total && total > 0 {
            self.selected_project_index = total - 1;
        }
    }

    fn start_project_refresh(&mut self) {
        if self.project_refresh_active {
            return;
        }

        self.project_refresh_active = true;
        self.project_refresh_generation = self.project_refresh_generation.wrapping_add(1);
        let generation = self.project_refresh_generation;
        let tx = self.project_refresh_tx.clone();
        let config = self.config.clone();

        self.rt.spawn(async move {
            let snapshot =
                tokio::task::spawn_blocking(move || build_project_refresh_snapshot(config))
                    .await
                    .unwrap_or_else(|e| ProjectRefreshSnapshot::worker_failed(e.to_string()));
            let _ = tx.send((generation, snapshot));
        });
    }

    fn apply_project_refresh_result(&mut self) {
        let (generation, snapshot) = match self.project_refresh_rx.try_recv() {
            Ok(result) => result,
            Err(_) => return,
        };
        self.project_refresh_active = false;
        if generation != self.project_refresh_generation {
            return;
        }
        self.apply_project_refresh_snapshot(snapshot);
    }

    /// Refresh the agent list, filtered by `current_project` if set.
    pub fn refresh_agents(&mut self) {
        let prev_row_key = self.selected_project_detail_row_key();
        if self.current_project.as_deref() == Some("(unassigned)") {
            self.agents.clear();
            self.agent_activity.clear();
            if !self.restore_project_detail_selection(prev_row_key.as_ref()) {
                self.clamp_project_detail_selection();
            }
            return;
        }

        self.agents = if let Some(project) = self.current_project.as_deref() {
            use_cases::unattached_agents_for_project(&self.config, project).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to list agents");
                Vec::new()
            })
        } else {
            use_cases::list_agents(&self.config, None, None).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to list agents");
                Vec::new()
            })
        };
        self.agents
            .retain(|agent| agent.meta.status != AgentStatus::Archived);
        self.agents
            .sort_by(|a, b| b.meta.created_at.cmp(&a.meta.created_at));
        if !self.restore_project_detail_selection(prev_row_key.as_ref()) {
            self.clamp_project_detail_selection();
        }
        self.refresh_agent_activity();
    }

    fn refresh_agent_activity(&mut self) {
        let active_sessions: HashSet<String> = self
            .agents
            .iter()
            .chain(self.attached_task_agents.values().flatten())
            .map(Self::agent_session_name)
            .collect();
        self.refresh_agent_activity_for_sessions(active_sessions);
    }

    fn refresh_agent_activity_for_sessions(&mut self, active_sessions: HashSet<String>) {
        if active_sessions.is_empty() {
            self.agent_activity.clear();
            return;
        }

        let windows = match Tmux::list_window_activity() {
            Ok(windows) => windows,
            Err(e) => {
                if !self.agent_activity_query_failed_logged {
                    tracing::warn!(
                        error = %e,
                        "failed to query tmux activity; agent statuses will be idle"
                    );
                    self.agent_activity_query_failed_logged = true;
                }
                self.agent_activity.clear();
                return;
            }
        };
        self.agent_activity_query_failed_logged = false;

        let mut by_session: HashMap<String, TmuxWindowActivity> = HashMap::new();
        for activity in windows {
            if !active_sessions.contains(&activity.session_name) {
                continue;
            }
            let replace = by_session
                .get(&activity.session_name)
                .map(|current| activity.window_activity > current.window_activity)
                .unwrap_or(true);
            if replace {
                by_session.insert(activity.session_name.clone(), activity);
            }
        }

        self.agent_activity.retain(|session, _| {
            active_sessions.contains(session) && by_session.contains_key(session)
        });

        let observed_at = Instant::now();
        let now_epoch_secs = unix_epoch_secs();
        for session in active_sessions {
            if let Some(activity) = by_session.get(&session) {
                self.agent_activity.insert(
                    session,
                    AgentActivitySample::from_tmux_window(activity, observed_at, now_epoch_secs),
                );
            } else {
                self.agent_activity.remove(&session);
            }
        }
    }

    pub fn agent_activity_sample(&self, session_name: &str) -> Option<&AgentActivitySample> {
        self.agent_activity.get(session_name)
    }

    /// Total entries in the project list (projects + unassigned pseudo-entry).
    pub fn project_list_len(&self) -> usize {
        self.projects.len() + if self.unassigned_task_count > 0 { 1 } else { 0 }
    }

    /// Refresh tasks filtered by current_project.
    fn refresh_tasks_for_project(&mut self) {
        let prev_row_key = self.selected_project_detail_row_key();
        let all = Task::list_all(&self.config);
        let in_progress = &self.archive_in_progress;
        self.tasks = match &self.current_project {
            Some(name) if name == "(unassigned)" => all
                .into_iter()
                .filter(|t| t.meta.project.is_none())
                .filter(|t| !in_progress.contains(&t.meta.task_id()))
                .collect(),
            Some(name) => all
                .into_iter()
                .filter(|t| t.meta.project.as_deref() == Some(name.as_str()))
                .filter(|t| !in_progress.contains(&t.meta.task_id()))
                .collect(),
            None => all
                .into_iter()
                .filter(|t| !in_progress.contains(&t.meta.task_id()))
                .collect(),
        };
        self.refresh_attached_task_agents();
        if self.restore_project_detail_selection(prev_row_key.as_ref()) {
            return;
        }
        self.clamp_project_detail_selection();
    }

    fn refresh_attached_task_agents(&mut self) {
        self.attached_task_agents.clear();
        for task in &self.tasks {
            let task_id = task.meta.task_id();
            match use_cases::attached_agents_for_task(&self.config, &task_id) {
                Ok(agents) => {
                    self.attached_task_agents.insert(task_id, agents);
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to list attached task agents");
                }
            }
        }
    }

    pub fn project_detail_rows(&self) -> Vec<ProjectDetailRow<'_>> {
        let mut rows = Vec::new();
        rows.push(ProjectDetailRow::AgentsSectionHeader);
        rows.push(ProjectDetailRow::SectionColumnSpacer);
        rows.push(ProjectDetailRow::AgentsColumnsHeader);
        if self.agents.is_empty() {
            rows.push(ProjectDetailRow::EmptyAgents);
        } else {
            rows.extend(
                self.agents
                    .iter()
                    .map(|agent| ProjectDetailRow::UnattachedAgent { agent }),
            );
        }

        rows.push(ProjectDetailRow::SectionSpacer);
        rows.push(ProjectDetailRow::TasksSectionHeader);
        rows.push(ProjectDetailRow::SectionColumnSpacer);
        rows.push(ProjectDetailRow::TasksColumnsHeader);
        if self.tasks.is_empty() {
            rows.push(ProjectDetailRow::EmptyTasks);
        } else {
            for (task_index, task) in self.tasks.iter().enumerate() {
                rows.push(ProjectDetailRow::Task(ProjectTaskRow::Task {
                    task_index,
                    task,
                }));
                if let Some(agents) = self.attached_task_agents.get(&task.meta.task_id()) {
                    if !agents.is_empty() {
                        rows.push(ProjectDetailRow::AttachedAgentsHeader);
                        rows.extend(agents.iter().map(|agent| {
                            ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent {
                                task_index,
                                agent,
                            })
                        }));
                    }
                }
                if task_index + 1 < self.tasks.len() {
                    rows.push(ProjectDetailRow::TaskGroupSpacer);
                }
            }
        }
        rows
    }

    pub fn project_detail_row_count(&self) -> usize {
        self.project_detail_rows().len()
    }

    pub fn selected_project_detail_row(&self) -> Option<ProjectDetailRow<'_>> {
        self.project_detail_rows().get(self.selected_index).copied()
    }

    fn selected_project_detail_row_key(&self) -> Option<ProjectDetailRowKey> {
        let row = self.selected_project_detail_row()?;
        self.project_detail_row_key(row)
    }

    fn project_detail_row_key(&self, row: ProjectDetailRow<'_>) -> Option<ProjectDetailRowKey> {
        match row {
            ProjectDetailRow::UnattachedAgent { agent, .. } => {
                Some(ProjectDetailRowKey::UnattachedAgent {
                    project: agent.meta.project.clone(),
                    name: agent.meta.name.clone(),
                })
            }
            ProjectDetailRow::Task(task_row) => self
                .project_task_row_key(task_row)
                .map(ProjectDetailRowKey::Task),
            ProjectDetailRow::AttachedAgent(task_row) => self
                .project_task_row_key(task_row)
                .map(ProjectDetailRowKey::AttachedAgent),
            ProjectDetailRow::AgentsSectionHeader
            | ProjectDetailRow::SectionColumnSpacer
            | ProjectDetailRow::AgentsColumnsHeader
            | ProjectDetailRow::EmptyAgents
            | ProjectDetailRow::SectionSpacer
            | ProjectDetailRow::TaskGroupSpacer
            | ProjectDetailRow::TasksSectionHeader
            | ProjectDetailRow::TasksColumnsHeader
            | ProjectDetailRow::EmptyTasks
            | ProjectDetailRow::AttachedAgentsHeader => None,
        }
    }

    fn project_detail_agent_key(agent: &AgentRecord) -> ProjectDetailRowKey {
        match &agent.meta.attachment {
            AgentAttachment::Task { task_id, .. } => {
                ProjectDetailRowKey::AttachedAgent(ProjectTaskRowKey::Agent {
                    task_id: task_id.clone(),
                    project: agent.meta.project.clone(),
                    name: agent.meta.name.clone(),
                })
            }
            AgentAttachment::Unattached => ProjectDetailRowKey::UnattachedAgent {
                project: agent.meta.project.clone(),
                name: agent.meta.name.clone(),
            },
        }
    }

    fn restore_project_detail_selection(&mut self, key: Option<&ProjectDetailRowKey>) -> bool {
        let Some(key) = key else {
            return false;
        };
        if let Some(idx) = self.find_project_detail_row_key(key) {
            self.selected_index = idx;
            return true;
        }
        if let ProjectDetailRowKey::AttachedAgent(ProjectTaskRowKey::Agent { task_id, .. }) = key {
            if let Some(idx) = self.find_project_detail_row_key(&ProjectDetailRowKey::Task(
                ProjectTaskRowKey::Task {
                    task_id: task_id.clone(),
                },
            )) {
                self.selected_index = idx;
                return true;
            }
        }
        false
    }

    fn find_project_detail_row_key(&self, key: &ProjectDetailRowKey) -> Option<usize> {
        self.project_detail_rows()
            .iter()
            .position(|row| self.project_detail_row_key(*row).as_ref() == Some(key))
    }

    fn clamp_project_detail_selection(&mut self) {
        self.clamp_project_detail_selection_near(self.selected_index);
    }

    fn clamp_project_detail_selection_near(&mut self, preferred_index: usize) {
        if self.project_detail_row_count() == 0 {
            self.selected_index = 0;
            return;
        }
        let rows = self.project_detail_rows();
        if rows
            .get(self.selected_index)
            .is_some_and(Self::project_detail_row_is_actionable)
        {
            return;
        }

        let start = preferred_index.min(rows.len().saturating_sub(1));
        if let Some(idx) = (0..=start)
            .rev()
            .find(|idx| Self::project_detail_row_is_actionable(&rows[*idx]))
        {
            self.selected_index = idx;
        } else if let Some(idx) = (start.saturating_add(1)..rows.len())
            .find(|idx| Self::project_detail_row_is_actionable(&rows[*idx]))
        {
            self.selected_index = idx;
        } else {
            self.selected_index = 0;
        }
    }

    fn project_detail_row_is_actionable(row: &ProjectDetailRow<'_>) -> bool {
        matches!(
            row,
            ProjectDetailRow::UnattachedAgent { .. }
                | ProjectDetailRow::Task(_)
                | ProjectDetailRow::AttachedAgent(_)
        )
    }

    fn next_project_detail_row(&mut self) {
        self.move_project_detail_selection(1);
    }

    fn previous_project_detail_row(&mut self) {
        self.move_project_detail_selection(-1);
    }

    fn move_project_detail_selection(&mut self, delta: isize) {
        let rows = self.project_detail_rows();
        let actionable: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(idx, row)| Self::project_detail_row_is_actionable(row).then_some(idx))
            .collect();
        if actionable.is_empty() {
            self.selected_index = 0;
            return;
        }
        let current = actionable
            .iter()
            .position(|idx| *idx == self.selected_index)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(actionable.len() as isize) as usize;
        self.selected_index = actionable[next];
    }

    fn select_first_project_detail_row(&mut self) {
        let rows = self.project_detail_rows();
        if let Some(idx) = rows.iter().position(Self::project_detail_row_is_actionable) {
            self.selected_index = idx;
        } else {
            self.selected_index = 0;
        }
    }

    fn select_last_project_detail_row(&mut self) {
        let rows = self.project_detail_rows();
        if let Some(idx) = rows
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, row)| Self::project_detail_row_is_actionable(row).then_some(idx))
        {
            self.selected_index = idx;
        } else {
            self.selected_index = 0;
        }
    }

    fn jump_to_next_project_detail_section(&mut self) {
        let rows = self.project_detail_rows();
        let tasks_start = rows
            .iter()
            .position(|row| matches!(row, ProjectDetailRow::TasksSectionHeader))
            .unwrap_or(0);
        let in_agents_section = self.selected_index < tasks_start;
        let mut search_range = if in_agents_section {
            tasks_start.saturating_add(1)..rows.len()
        } else {
            1..tasks_start
        };
        if let Some(idx) = search_range.find(|idx| {
            rows.get(*idx)
                .is_some_and(Self::project_detail_row_is_actionable)
        }) {
            self.selected_index = idx;
        }
    }

    fn jump_to_previous_project_detail_section(&mut self) {
        let rows = self.project_detail_rows();
        let tasks_start = rows
            .iter()
            .position(|row| matches!(row, ProjectDetailRow::TasksSectionHeader))
            .unwrap_or(0);
        let in_tasks_section = self.selected_index > tasks_start;
        let mut search_range = if in_tasks_section {
            1..tasks_start
        } else {
            tasks_start.saturating_add(1)..rows.len()
        };
        if let Some(idx) = search_range.find(|idx| {
            rows.get(*idx)
                .is_some_and(Self::project_detail_row_is_actionable)
        }) {
            self.selected_index = idx;
        }
    }

    fn selected_task_index(&self) -> Option<usize> {
        match self.selected_project_detail_row() {
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { task_index, .. })) => {
                Some(task_index)
            }
            None => None,
            _ => None,
        }
    }

    #[cfg(test)]
    fn selected_project_task_row(&self) -> Option<ProjectTaskRow<'_>> {
        match self.selected_project_detail_row() {
            Some(ProjectDetailRow::Task(row) | ProjectDetailRow::AttachedAgent(row)) => Some(row),
            _ => None,
        }
    }

    fn project_task_row_key(&self, row: ProjectTaskRow<'_>) -> Option<ProjectTaskRowKey> {
        match row {
            ProjectTaskRow::Task { task, .. } => Some(ProjectTaskRowKey::Task {
                task_id: task.meta.task_id(),
            }),
            ProjectTaskRow::Agent { task_index, agent } => {
                let task_id = self.tasks.get(task_index)?.meta.task_id();
                Some(ProjectTaskRowKey::Agent {
                    task_id,
                    project: agent.meta.project.clone(),
                    name: agent.meta.name.clone(),
                })
            }
        }
    }

    pub fn selected_project_detail_agent(&self) -> Option<&AgentRecord> {
        match self.selected_project_detail_row() {
            Some(ProjectDetailRow::UnattachedAgent { agent, .. })
            | Some(ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })) => {
                Some(agent)
            }
            _ => None,
        }
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.selected_task_index()
            .and_then(|task_index| self.tasks.get(task_index))
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

    pub fn clear_old_output(&mut self) {
        if let Some(instant) = &self.last_output_time {
            if instant.elapsed() > Duration::from_secs(7) {
                self.output_log.clear();
                self.output_scroll = 0;
                self.last_output_time = None;
            }
        }
    }

    fn agent_session_name(agent: &AgentRecord) -> String {
        match &agent.meta.kind {
            AgentKind::Engineer => {
                Config::engineer_tmux_session(&agent.meta.project, &agent.meta.name)
            }
            AgentKind::Researcher { .. } => {
                Config::researcher_tmux_session(&agent.meta.project, &agent.meta.name)
            }
            AgentKind::Operator { .. } => {
                Config::operator_tmux_session(&agent.meta.project, &agent.meta.name)
            }
            AgentKind::Reviewer { .. } => {
                Config::reviewer_tmux_session(&agent.meta.project, &agent.meta.name)
            }
            AgentKind::Tester { .. } => {
                Config::tester_tmux_session(&agent.meta.project, &agent.meta.name)
            }
        }
    }

    fn agent_kind_label(kind: &AgentKind) -> &'static str {
        match kind {
            AgentKind::Engineer => "engineer",
            AgentKind::Researcher { .. } => "researcher",
            AgentKind::Operator { .. } => "operator",
            AgentKind::Reviewer { .. } => "reviewer",
            AgentKind::Tester { .. } => "tester",
        }
    }

    fn open_agent(&mut self, agent: &AgentRecord) {
        if self.popup.is_some() {
            return;
        }

        let project = agent.meta.project.clone();
        let name = agent.meta.name.clone();
        let session_name = Self::agent_session_name(agent);

        if !Tmux::session_exists(&session_name) {
            match use_cases::resume_agent(&self.config, &project, &name) {
                Ok(()) => {
                    tracing::info!(session = &session_name, "resumed agent session");
                    self.refresh_agents();
                }
                Err(e) => {
                    self.set_status(format!("Failed to resume: {e}"));
                    return;
                }
            }
        }

        match Tmux::popup_attach(&session_name) {
            Ok(child) => {
                tracing::info!(session = &session_name, "attached to agent session");
                self.popup = Some(ActivePopup { child });
            }
            Err(e) => {
                self.set_status(format!("Failed to attach: {e}"));
            }
        }
    }

    fn start_agent_wizard(&mut self) {
        let Some(project) = self.current_project.clone() else {
            self.set_status("No project available".to_string());
            return;
        };
        if project == "(unassigned)" {
            self.set_status("No agents for unassigned tasks".to_string());
            return;
        }

        tracing::info!(project = %project, "opening agent wizard");
        let mut name_editor = TextArea::default();
        name_editor.set_cursor_line_style(ratatui::style::Style::default());
        self.agent_wizard = Some(AgentWizard {
            kind: AgentWizardKind::Researcher,
            step: AgentWizardStep::Kind,
            name_editor,
            description_editor: VimTextArea::new(),
            worktree_rows: vec![ReviewerWorktreeRow::new()],
            selected_row: 0,
            browser_capability: false,
            error_message: None,
            project,
        });
        self.view = View::AgentWizard;
    }

    fn archive_agent_record(&mut self, agent: &AgentRecord) {
        let project = agent.meta.project.clone();
        let name = agent.meta.name.clone();

        match use_cases::archive_agent(&self.config, &project, &name) {
            Ok(()) => {
                self.set_status(format!("Archived agent '{name}'"));
                self.refresh_agents();
                self.refresh_tasks_for_project();
            }
            Err(e) => {
                self.set_status(format!("Failed to archive: {e}"));
            }
        }
    }

    fn open_project_pm_chat(&mut self) {
        let Some(project_name) = self.current_project.clone() else {
            return;
        };
        if project_name == "(unassigned)" || self.popup.is_some() {
            return;
        }

        match use_cases::open_pm_popup(&self.config, &project_name) {
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

    fn open_project_notes(&mut self, project: &str, return_view: View) {
        match NotesView::new(self.config.project_notes_dir(project)) {
            Ok(nv) => {
                tracing::info!(project = %project, "opening project notes view");
                self.notes_view = Some(nv);
                self.notes_return_view = return_view;
                self.view = View::Notes;
            }
            Err(e) => {
                self.set_status(format!("Failed to open notes: {e}"));
            }
        }
    }

    fn open_global_notes(&mut self, return_view: View) {
        match NotesView::new(self.config.notes_dir.clone()) {
            Ok(nv) => {
                tracing::info!("opening global notes view");
                self.notes_view = Some(nv);
                self.notes_return_view = return_view;
                self.view = View::Notes;
            }
            Err(e) => {
                self.set_status(format!("Failed to open notes: {e}"));
            }
        }
    }

    fn close_notes(&mut self) {
        self.notes_view = None;
        self.view = self.notes_return_view;
    }

    fn start_project_respawn_confirm(&mut self) {
        let Some(project_name) = self.current_project.clone() else {
            return;
        };
        if project_name == "(unassigned)" || self.respawn_in_progress.is_some() {
            return;
        }

        self.respawn_confirm_target = Some(project_name);
        self.respawn_confirm_index = 0;
        self.respawn_confirm_is_chief_of_staff = false;
        self.respawn_confirm_return_view = View::TaskList;
        self.view = View::RespawnConfirm;
    }

    fn start_focused_archive_confirm(&mut self) {
        let has_selection = matches!(
            self.selected_project_detail_row(),
            Some(ProjectDetailRow::Task(_))
                | Some(ProjectDetailRow::UnattachedAgent { .. })
                | Some(ProjectDetailRow::AttachedAgent(_))
        );
        if has_selection {
            self.view = View::DeleteConfirm;
        }
    }

    fn archive_focused_project_row(&mut self) -> Result<()> {
        match self.selected_project_detail_row() {
            Some(ProjectDetailRow::Task(_)) => self.archive_task(false)?,
            Some(ProjectDetailRow::UnattachedAgent { agent, .. })
            | Some(ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })) => {
                let agent = agent.clone();
                self.archive_agent_record(&agent);
                self.view = View::TaskList;
            }
            _ => {}
        }
        Ok(())
    }

    fn open_archive(&mut self, kind: ArchiveKind) {
        self.archive_kind = kind;
        match kind {
            ArchiveKind::Tasks => {
                self.archive_tasks = if let Some(project) = self.current_project.as_deref() {
                    use_cases::list_archived_tasks(&self.config, project)
                } else {
                    Vec::new()
                };
                self.archive_agents.clear();
            }
            ArchiveKind::Agents => {
                self.archive_agents = if let Some(project) = self.current_project.as_deref() {
                    if project == "(unassigned)" {
                        Vec::new()
                    } else {
                        use_cases::list_archived_agents(&self.config, project)
                    }
                } else {
                    Vec::new()
                };
                self.archive_tasks.clear();
            }
        }
        self.archive_search = Self::create_plain_editor();
        self.archive_selected = 0;
        self.archive_preview = None;
        self.archive_scroll = 0;
        self.view = View::Archive;
    }

    fn return_from_agent_wizard(&mut self) {
        self.agent_wizard = None;
        self.refresh_agents();
        self.view = View::TaskList;
    }

    fn load_preview(&mut self) {
        let (preview_content, notes_content) = if let Some(task) = self.selected_task() {
            let preview = task
                .read_agent_log_structured_tail(500)
                .unwrap_or_else(|_| "No agent log available".to_string());
            let notes = task.read_notes().unwrap_or_default();
            (preview, notes)
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

    fn archive_task(&mut self, saved: bool) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }

        let Some(task_index) = self.selected_task_index() else {
            return Ok(());
        };
        let task = self.tasks.remove(task_index);
        let task_id = task.meta.task_id();

        tracing::info!(task_id = %task_id, saved, "TUI: archive task requested (async)");
        self.log_output(format!("Archiving task {}...", task_id));

        // Optimistic UI updates: clear attached agent cache, clamp selection,
        // and mark the task as in-progress so refresh_tasks_for_project can
        // suppress re-adding the task while the worker rewrites archived_at.
        self.archive_in_progress.insert(task_id.clone());
        self.refresh_attached_task_agents();
        self.clamp_project_detail_selection_near(self.selected_index);

        let label = if saved {
            "Archiving & saving"
        } else {
            "Archiving"
        };
        self.set_status(format!("{}: {}", label, task_id));
        self.view = View::TaskList;

        // Offload the heavy archive work (tmux kills, worktree removal,
        // agent archiving, meta rewrite) so the TUI event loop keeps
        // ticking. Result is drained by `apply_archive_results`.
        let tx = self.archive_tx.clone();
        let config = self.config.clone();
        let mut owned_task = task;
        let task_id_for_send = task_id.clone();
        self.rt.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                use_cases::archive_task(&config, &mut owned_task, saved).map(|_| saved)
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
            let msg = match result {
                Ok(saved) => Ok(saved),
                Err(e) => Err(format!("{e}")),
            };
            let _ = tx.send((task_id_for_send, msg));
        });
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
            is_multi_repo: is_multi,
        });

        self.view = View::NewTaskWizard;
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
                if description.is_empty() && is_multi {
                    let wizard = self.wizard.as_mut().unwrap();
                    wizard.error_message =
                        Some("Multi-repo tasks require a description".to_string());
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
            self.log_output("  Launching engineer via supervisor...".to_string());
            if let Err(e) = supervisor::ensure_task_tmux(&self.config, &task)
                .and_then(|_| supervisor::launch_next_step(&self.config, &mut task).map(|_| ()))
            {
                tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to launch multi-repo task engineer");
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to launch engineer: {}", e));
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
            self.log_output("  Launching engineer via supervisor...".to_string());
            if let Err(e) = supervisor::ensure_task_tmux(&self.config, &task)
                .and_then(|_| supervisor::launch_next_step(&self.config, &mut task).map(|_| ()))
            {
                tracing::error!(repo = %name, branch = %branch_name, error = %e, "failed to launch task engineer");
                self.log_output(format!("  Error: {}", e));
                if let Some(w) = &mut self.wizard {
                    w.error_message = Some(format!("Failed to launch engineer: {}", e));
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

    pub fn handle_event(&mut self, event: Event) -> Result<bool> {
        self.clear_old_status();

        match self.view {
            View::ProjectList => self.handle_project_list_event(event),
            View::TaskList => self.handle_task_list_event(event),
            View::Preview => self.handle_preview_event(event),
            View::DeleteConfirm => self.handle_delete_confirm_event(event),
            View::NewTaskWizard => self.handle_wizard_event(event),
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
            View::AgentWizard => self.handle_agent_wizard_event(event),
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
                        self.refresh_agents();
                        self.clamp_project_detail_selection();
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
                KeyCode::Char('o') => {
                    self.open_global_notes(View::ProjectList);
                }
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
                _ => {
                    self.handle_project_detail_key(key)?;
                }
            }
        }
        Ok(false)
    }

    fn handle_project_detail_key(&mut self, key: event::KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('k') | KeyCode::Up => {
                self.previous_project_detail_row();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.next_project_detail_row();
            }
            KeyCode::Tab => {
                self.jump_to_next_project_detail_section();
            }
            KeyCode::BackTab => {
                self.jump_to_previous_project_detail_section();
            }
            KeyCode::Enter => match self.selected_project_detail_row() {
                Some(ProjectDetailRow::Task(_)) => {
                    self.load_preview();
                    self.preview_pane = PreviewPane::Logs;
                    self.view = View::Preview;
                }
                Some(ProjectDetailRow::UnattachedAgent { .. })
                | Some(ProjectDetailRow::AttachedAgent(_)) => {
                    let Some(agent) = self.selected_project_detail_agent().cloned() else {
                        return Ok(false);
                    };
                    self.open_agent(&agent);
                }
                _ => {}
            },
            KeyCode::Char('d') => {
                self.start_focused_archive_confirm();
            }
            KeyCode::Char('G') => {
                self.select_last_project_detail_row();
            }
            KeyCode::Char('g') => {
                self.select_first_project_detail_row();
            }
            KeyCode::Char('n') => {
                self.start_wizard()?;
            }
            KeyCode::Char('a') => {
                self.start_agent_wizard();
            }
            KeyCode::Char('p') => {
                let pr_info = match self.selected_project_detail_row() {
                    Some(ProjectDetailRow::Task(_)) => self.selected_task().and_then(|t| {
                        t.meta
                            .linked_pr
                            .as_ref()
                            .map(|pr| (pr.number, pr.url.clone()))
                    }),
                    _ => None,
                };
                if let Some((number, url)) = pr_info {
                    open_url(&url);
                    self.set_status(format!("Opening PR #{}...", number));
                } else {
                    self.set_status("No linked PR".to_string());
                }
            }
            KeyCode::Char('o') => {
                let Some(project) = self.current_project.clone() else {
                    self.set_status("No project selected".to_string());
                    return Ok(false);
                };
                if project == "(unassigned)" {
                    self.set_status("Unassigned tasks do not have project notes".to_string());
                } else {
                    self.open_project_notes(&project, View::TaskList);
                }
            }
            KeyCode::Char('r') => {
                if matches!(
                    self.selected_project_detail_row(),
                    Some(ProjectDetailRow::Task(_))
                ) {
                    self.restart_selected_task()?;
                }
            }
            KeyCode::Char('c') => {
                self.open_project_pm_chat();
            }
            KeyCode::Char('z') => {
                let kind = match self.selected_project_detail_row() {
                    Some(ProjectDetailRow::UnattachedAgent { .. })
                    | Some(ProjectDetailRow::AttachedAgent(_)) => ArchiveKind::Agents,
                    _ => ArchiveKind::Tasks,
                };
                self.open_archive(kind);
            }
            KeyCode::Char('e') => {
                self.start_project_respawn_confirm();
            }
            _ => {}
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

    /// Return indices into the active archive list that match the current search query.
    pub fn archive_filtered_indices(&self) -> Vec<usize> {
        let query: String = self.archive_search.lines().join("").to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        match self.archive_kind {
            ArchiveKind::Tasks => {
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
            ArchiveKind::Agents => {
                if terms.is_empty() {
                    return (0..self.archive_agents.len()).collect();
                }
                self.archive_agents
                    .iter()
                    .enumerate()
                    .filter(|(_, (agent, content))| {
                        let name_lower = agent.meta.name.to_lowercase();
                        let project_lower = agent.meta.project.to_lowercase();
                        let kind_lower = Self::agent_kind_label(&agent.meta.kind).to_lowercase();
                        let content_lower = content.to_lowercase();
                        terms.iter().all(|term| {
                            name_lower.contains(term)
                                || project_lower.contains(term)
                                || kind_lower.contains(term)
                                || content_lower.contains(term)
                        })
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        }
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

    fn handle_agent_wizard_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return Ok(false);
            }

            // Ctrl+S submits from any step. The wizard validates required
            // fields and surfaces inline errors so the user never has to
            // play guess-the-step.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                self.submit_agent_wizard();
                return Ok(false);
            }

            let global_harness = self.config.harness_kind();
            let Some(wizard) = self.agent_wizard.as_mut() else {
                return Ok(false);
            };
            wizard.error_message = None;

            match wizard.step {
                AgentWizardStep::Kind => match key.code {
                    KeyCode::Esc => {
                        self.return_from_agent_wizard();
                    }
                    KeyCode::Char('j')
                    | KeyCode::Down
                    | KeyCode::Char('l')
                    | KeyCode::Right
                    | KeyCode::Tab => {
                        wizard.kind = match wizard.kind {
                            AgentWizardKind::Researcher => AgentWizardKind::Reviewer,
                            AgentWizardKind::Reviewer => AgentWizardKind::Tester,
                            AgentWizardKind::Tester => AgentWizardKind::Operator,
                            AgentWizardKind::Operator => AgentWizardKind::Researcher,
                        };
                    }
                    KeyCode::Char('k')
                    | KeyCode::Up
                    | KeyCode::Char('h')
                    | KeyCode::Left
                    | KeyCode::BackTab => {
                        wizard.kind = match wizard.kind {
                            AgentWizardKind::Researcher => AgentWizardKind::Operator,
                            AgentWizardKind::Reviewer => AgentWizardKind::Researcher,
                            AgentWizardKind::Tester => AgentWizardKind::Reviewer,
                            AgentWizardKind::Operator => AgentWizardKind::Tester,
                        };
                    }
                    KeyCode::Enter => {
                        wizard.step = AgentWizardStep::Name;
                    }
                    _ => {}
                },
                AgentWizardStep::Name => {
                    if key.code == KeyCode::Esc {
                        // First content step — Esc cancels the wizard.
                        self.return_from_agent_wizard();
                        return Ok(false);
                    }
                    if key.code == KeyCode::Enter {
                        // Advance: reviewers go to worktrees, researchers
                        // skip straight to description.
                        wizard.step = match wizard.kind {
                            AgentWizardKind::Researcher | AgentWizardKind::Operator => {
                                AgentWizardStep::Description
                            }
                            AgentWizardKind::Reviewer | AgentWizardKind::Tester => {
                                AgentWizardStep::Worktrees
                            }
                        };
                        if matches!(wizard.step, AgentWizardStep::Description) {
                            wizard.description_editor.set_insert_mode();
                        }
                        return Ok(false);
                    }
                    let input = Input::from(event.clone());
                    wizard.name_editor.input(input);
                }
                AgentWizardStep::Worktrees => {
                    match key.code {
                        KeyCode::Esc => {
                            wizard.step = AgentWizardStep::Name;
                        }
                        KeyCode::Tab => {
                            // Tab cycles repo → branch → next row's repo.
                            let len = wizard.worktree_rows.len();
                            let cur = wizard.selected_row;
                            let row = &mut wizard.worktree_rows[cur];
                            if !row.branch_focus {
                                row.branch_focus = true;
                            } else if cur + 1 < len {
                                row.branch_focus = false;
                                wizard.selected_row += 1;
                            } else {
                                // Last field of last row — testers get an
                                // optional capabilities step first.
                                wizard.step = match wizard.kind {
                                    AgentWizardKind::Tester => AgentWizardStep::Capabilities,
                                    AgentWizardKind::Researcher
                                    | AgentWizardKind::Operator
                                    | AgentWizardKind::Reviewer => {
                                        wizard.description_editor.set_insert_mode();
                                        AgentWizardStep::Description
                                    }
                                };
                            }
                        }
                        KeyCode::BackTab => {
                            let row = &mut wizard.worktree_rows[wizard.selected_row];
                            if row.branch_focus {
                                row.branch_focus = false;
                            } else if wizard.selected_row > 0 {
                                wizard.selected_row -= 1;
                                wizard.worktree_rows[wizard.selected_row].branch_focus = true;
                            }
                        }
                        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Add a new row after the selected one.
                            wizard
                                .worktree_rows
                                .insert(wizard.selected_row + 1, ReviewerWorktreeRow::new());
                            wizard.selected_row += 1;
                            wizard.worktree_rows[wizard.selected_row].branch_focus = false;
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Remove the selected row, but never the last one.
                            if wizard.worktree_rows.len() > 1 {
                                wizard.worktree_rows.remove(wizard.selected_row);
                                if wizard.selected_row >= wizard.worktree_rows.len() {
                                    wizard.selected_row = wizard.worktree_rows.len() - 1;
                                }
                            }
                        }
                        KeyCode::Enter => {
                            // Enter advances field-by-field, mirroring Tab.
                            let len = wizard.worktree_rows.len();
                            let cur = wizard.selected_row;
                            let row = &mut wizard.worktree_rows[cur];
                            if !row.branch_focus {
                                row.branch_focus = true;
                            } else if cur + 1 < len {
                                row.branch_focus = false;
                                wizard.selected_row += 1;
                            } else {
                                wizard.step = match wizard.kind {
                                    AgentWizardKind::Tester => AgentWizardStep::Capabilities,
                                    AgentWizardKind::Researcher
                                    | AgentWizardKind::Operator
                                    | AgentWizardKind::Reviewer => {
                                        wizard.description_editor.set_insert_mode();
                                        AgentWizardStep::Description
                                    }
                                };
                            }
                        }
                        _ => {
                            let input = Input::from(event.clone());
                            let row = &mut wizard.worktree_rows[wizard.selected_row];
                            if row.branch_focus {
                                row.branch_editor.input(input);
                            } else {
                                row.repo_editor.input(input);
                            }
                        }
                    }
                }
                AgentWizardStep::Capabilities => match key.code {
                    KeyCode::Esc | KeyCode::BackTab => {
                        wizard.step = AgentWizardStep::Worktrees;
                    }
                    KeyCode::Enter | KeyCode::Tab => {
                        wizard.step = AgentWizardStep::Description;
                        wizard.description_editor.set_insert_mode();
                    }
                    KeyCode::Char(' ')
                    | KeyCode::Char('h')
                    | KeyCode::Char('l')
                    | KeyCode::Left
                    | KeyCode::Right => {
                        if !matches!(
                            global_harness,
                            agman::harness::HarnessKind::Goose | agman::harness::HarnessKind::Pi
                        ) {
                            wizard.browser_capability = !wizard.browser_capability;
                        }
                    }
                    _ => {}
                },
                AgentWizardStep::Description => {
                    let input = Input::from(event.clone());
                    let was_insert = wizard.description_editor.mode() == VimMode::Insert;
                    wizard.description_editor.input(input.clone());
                    let is_normal_now = wizard.description_editor.mode() == VimMode::Normal;
                    // Esc in normal mode steps back to the previous step.
                    if input.key == Key::Esc && !was_insert && is_normal_now {
                        wizard.step = match wizard.kind {
                            AgentWizardKind::Researcher | AgentWizardKind::Operator => {
                                AgentWizardStep::Name
                            }
                            AgentWizardKind::Reviewer => AgentWizardStep::Worktrees,
                            AgentWizardKind::Tester => AgentWizardStep::Capabilities,
                        };
                    }
                }
            }
        }
        Ok(false)
    }

    /// Validate and submit the agent wizard. On success the wizard closes
    /// and the new agent is started; on failure the error message is
    /// surfaced inline so the user can correct and resubmit.
    fn submit_agent_wizard(&mut self) {
        let Some(wizard) = self.agent_wizard.as_ref() else {
            return;
        };
        let kind = wizard.kind;
        let project = wizard.project.clone();
        let name = wizard.name_editor.lines().join("").trim().to_string();
        let desc = wizard.description_editor.lines_joined().trim().to_string();
        let browser_capability = wizard.browser_capability;

        if name.is_empty() {
            if let Some(w) = self.agent_wizard.as_mut() {
                w.error_message = Some("AgentRecord name is required".to_string());
                w.step = AgentWizardStep::Name;
            }
            return;
        }

        match kind {
            AgentWizardKind::Researcher | AgentWizardKind::Operator => {
                let (kind_name, create_result) = match kind {
                    AgentWizardKind::Researcher => (
                        "researcher",
                        use_cases::create_researcher(
                            &self.config,
                            &project,
                            &name,
                            &desc,
                            None,
                            None,
                            None,
                        ),
                    ),
                    AgentWizardKind::Operator => (
                        "operator",
                        use_cases::create_operator(
                            &self.config,
                            &project,
                            &name,
                            &desc,
                            None,
                            None,
                            None,
                        ),
                    ),
                    _ => unreachable!(),
                };
                match create_result {
                    Ok(_agent) => {
                        tracing::info!(project = %project, name = %name, kind = kind_name, "created agent via wizard");
                        if let Err(e) =
                            use_cases::start_agent_session(&self.config, &project, &name, false)
                        {
                            tracing::warn!(
                                project = %project, name = %name, error = %e,
                                "failed to start agent session"
                            );
                        }
                        self.set_status(format!("Created {kind_name}: {name}"));
                        self.return_from_agent_wizard();
                    }
                    Err(e) => {
                        tracing::warn!(project = %project, name = %name, kind = kind_name, error = %e, "failed to create agent");
                        if let Some(w) = self.agent_wizard.as_mut() {
                            w.error_message = Some(format!("{e}"));
                        }
                    }
                }
            }
            AgentWizardKind::Reviewer => {
                // Collect (repo, branch) rows; reject empties so we don't
                // hand a half-filled row to the use-case.
                let mut branches: Vec<(String, String)> = Vec::new();
                for row in &wizard.worktree_rows {
                    let r = row.repo();
                    let b = row.branch();
                    if r.is_empty() && b.is_empty() {
                        continue;
                    }
                    if r.is_empty() || b.is_empty() {
                        if let Some(w) = self.agent_wizard.as_mut() {
                            w.error_message = Some(
                                "Each row needs both a repo and a branch (Ctrl+D to remove)"
                                    .to_string(),
                            );
                            w.step = AgentWizardStep::Worktrees;
                        }
                        return;
                    }
                    branches.push((r, b));
                }
                if branches.is_empty() {
                    if let Some(w) = self.agent_wizard.as_mut() {
                        w.error_message =
                            Some("Reviewer needs at least one (repo, branch) pair".to_string());
                        w.step = AgentWizardStep::Worktrees;
                    }
                    return;
                }

                let spec = use_cases::WorktreeSpec {
                    branches,
                    parent_dir: None,
                };
                match use_cases::create_reviewer(&self.config, &project, &name, &desc, spec) {
                    Ok(_agent) => {
                        tracing::info!(project = %project, name = %name, "created reviewer via wizard");
                        if let Err(e) =
                            use_cases::start_agent_session(&self.config, &project, &name, false)
                        {
                            tracing::warn!(
                                project = %project, name = %name, error = %e,
                                "failed to start agent session"
                            );
                        }
                        self.set_status(format!("Created reviewer: {name}"));
                        self.return_from_agent_wizard();
                    }
                    Err(e) => {
                        tracing::warn!(project = %project, name = %name, error = %e, "failed to create reviewer");
                        // The three-step decision tree raises a loud,
                        // user-actionable error for the local-branch case;
                        // surface it verbatim so the user can either fix
                        // the branch state or pick a different one.
                        if let Some(w) = self.agent_wizard.as_mut() {
                            w.error_message = Some(format!("{e}"));
                            w.step = AgentWizardStep::Worktrees;
                        }
                    }
                }
            }
            AgentWizardKind::Tester => {
                let mut branches: Vec<(String, String)> = Vec::new();
                for row in &wizard.worktree_rows {
                    let r = row.repo();
                    let b = row.branch();
                    if r.is_empty() && b.is_empty() {
                        continue;
                    }
                    if r.is_empty() || b.is_empty() {
                        if let Some(w) = self.agent_wizard.as_mut() {
                            w.error_message = Some(
                                "Each row needs both a repo and a branch (Ctrl+D to remove)"
                                    .to_string(),
                            );
                            w.step = AgentWizardStep::Worktrees;
                        }
                        return;
                    }
                    branches.push((r, b));
                }
                if branches.is_empty() {
                    if let Some(w) = self.agent_wizard.as_mut() {
                        w.error_message =
                            Some("Tester needs at least one (repo, branch) pair".to_string());
                        w.step = AgentWizardStep::Worktrees;
                    }
                    return;
                }

                let spec = use_cases::WorktreeSpec {
                    branches,
                    parent_dir: None,
                };
                let capabilities = agman::agent_model::TesterCapabilities {
                    browser: browser_capability,
                };
                match use_cases::create_tester(
                    &self.config,
                    &project,
                    &name,
                    &desc,
                    spec,
                    capabilities,
                ) {
                    Ok(_agent) => {
                        tracing::info!(project = %project, name = %name, "created tester via wizard");
                        if let Err(e) =
                            use_cases::start_agent_session(&self.config, &project, &name, false)
                        {
                            tracing::warn!(
                                project = %project, name = %name, error = %e,
                                "failed to start agent session"
                            );
                        }
                        self.set_status(format!("Created tester: {name}"));
                        self.return_from_agent_wizard();
                    }
                    Err(e) => {
                        tracing::warn!(project = %project, name = %name, error = %e, "failed to create tester");
                        if let Some(w) = self.agent_wizard.as_mut() {
                            w.error_message = Some(format!("{e}"));
                            w.step = AgentWizardStep::Worktrees;
                        }
                    }
                }
            }
        }
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
                    if let Some(&archive_idx) = filtered.get(self.archive_selected) {
                        let content = match self.archive_kind {
                            ArchiveKind::Tasks => self.archive_tasks[archive_idx].1.clone(),
                            ArchiveKind::Agents => self.archive_agents[archive_idx].1.clone(),
                        };
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
                if self.archive_kind == ArchiveKind::Tasks {
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
                    };
                }
            }
            KeyCode::Char('d') => {
                match self.archive_kind {
                    ArchiveKind::Tasks => {
                        // Permanently delete
                        let filtered = self.archive_filtered_indices();
                        if let Some(&task_idx) = filtered.get(self.archive_selected) {
                            let (task, _) = self.archive_tasks.remove(task_idx);
                            let task_id = task.meta.task_id();
                            if let Err(e) =
                                use_cases::permanently_delete_archived_task(&self.config, task)
                            {
                                tracing::error!(task_id = %task_id, error = %e, "failed to permanently delete archived task");
                                self.set_status(format!("Delete failed: {e}"));
                            } else {
                                self.set_status(format!("Deleted: {}", task_id));
                            }
                        }
                    }
                    ArchiveKind::Agents => {
                        let filtered = self.archive_filtered_indices();
                        if let Some(&agent_idx) = filtered.get(self.archive_selected) {
                            let (agent, _) = self.archive_agents.remove(agent_idx);
                            let project = agent.meta.project.clone();
                            let name = agent.meta.name.clone();
                            if let Err(e) =
                                use_cases::permanently_delete_archived_agent(&self.config, agent)
                            {
                                tracing::error!(project = %project, name = %name, error = %e, "failed to permanently delete archived agent");
                                self.set_status(format!("Delete failed: {e}"));
                            } else {
                                self.set_status(format!("Deleted: {project}--{name}"));
                            }
                        }
                    }
                }
                self.archive_preview = None;
                // Clamp selection
                let filtered = self.archive_filtered_indices();
                if self.archive_selected >= filtered.len() && !filtered.is_empty() {
                    self.archive_selected = filtered.len() - 1;
                }
            }
            KeyCode::Char('n') => {
                match self.archive_kind {
                    ArchiveKind::Tasks => {
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
                                self.set_status(format!(
                                    "Repo path not found: {}",
                                    repo_path.display()
                                ));
                            } else {
                                tracing::info!(task_id = %task_id, repo = %repo_name, "starting new task from archived task");
                                self.archive_preview = None;
                                self.create_wizard_from_picker(repo_name, repo_path, false)?;

                                let prefill = format!(
                                    "Reference: archived task \"{}\" (branch: {}). Examine the git branch/PR if needed.\n\n\n",
                                    task_id, branch_name,
                                );
                                if let Some(wizard) = self.wizard.as_mut() {
                                    wizard.description_editor.textarea.insert_str(&prefill);
                                }
                            }
                        }
                    }
                    ArchiveKind::Agents => {
                        let filtered = self.archive_filtered_indices();
                        if let Some(&agent_idx) = filtered.get(self.archive_selected) {
                            let agent = &self.archive_agents[agent_idx].0;
                            let project = agent.meta.project.clone();
                            let name = agent.meta.name.clone();
                            let key = Self::project_detail_agent_key(agent);
                            match use_cases::resume_agent(&self.config, &project, &name) {
                                Ok(()) => {
                                    self.set_status(format!("Restored agent '{name}'"));
                                    self.archive_preview = None;
                                    self.view = View::TaskList;
                                    self.refresh_tasks_for_project();
                                    self.refresh_agents();
                                    self.restore_project_detail_selection(Some(&key));
                                }
                                Err(e) => {
                                    tracing::error!(project = %project, name = %name, error = %e, "failed to restore archived agent");
                                    self.set_status(format!("Restore failed: {e}"));
                                }
                            }
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

            // Tab/BackTab to switch preview panes
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
                KeyCode::Char('p') => {
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
                    self.restart_selected_task()?;
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

    fn handle_delete_confirm_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Enter => self.archive_focused_project_row()?,
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
                            self.close_notes();
                        }
                    }
                    KeyCode::Char('q') => {
                        let _ = nv.save_current();
                        self.close_notes();
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
                    } else if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) && is_normal {
                        let _ = nv.save_current();
                        self.close_notes();
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

    fn restart_selected_task(&mut self) -> Result<()> {
        if self.tasks.is_empty() {
            return Ok(());
        }
        let task_id = match self.selected_task() {
            Some(task) if task.meta.has_repos() || task.meta.is_multi_repo() => task.meta.task_id(),
            Some(task) => {
                self.set_status(format!(
                    "Task {} has no repos configured",
                    task.meta.task_id()
                ));
                return Ok(());
            }
            None => return Ok(()),
        };
        let task_idx = match self
            .tasks
            .iter()
            .position(|task| task.meta.task_id() == task_id)
        {
            Some(idx) => idx,
            None => return Ok(()),
        };
        let launch_error = supervisor::ensure_task_tmux(&self.config, &self.tasks[task_idx])
            .and_then(|_| {
                supervisor::launch_next_step(&self.config, &mut self.tasks[task_idx]).map(|_| ())
            })
            .err();
        match launch_error {
            None => self.set_status(format!("Restarted engineer for {}", task_id)),
            Some(e) => self.set_status(format!("Restart failed: {}", e)),
        }
        self.refresh_tasks_and_select(&task_id);
        Ok(())
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
                    // In RepoSelect mode: Enter on a git repo or favourite selects it directly
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
                        Some(DirPickerOrigin::NewTask) => {
                            // Select current directory as repos_dir (fallback mode)
                            if let Some(picker) = self.dir_picker.take() {
                                let selected_dir = picker.current_dir.clone();

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

                                self.start_wizard()?;
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

    /// Handle a repo selection from the directory picker (RepoSelect mode).
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
                .map(|t| (t.name, t.inbox_path, t.seq_path, t.session_name, t.window))
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
                for (target, inbox_path, seq_path, session_name, window) in targets {
                    let window_ref = window.as_deref();

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

                    let visibility = match Tmux::is_window_visible_to_any_client(
                        &session_name,
                        window_ref,
                    ) {
                        Ok(visible) => Some(visible),
                        Err(e) => {
                            tracing::debug!(
                                target_name = &target,
                                session = &session_name,
                                error = %e,
                                "target visibility check failed; treating fresh messages as visible"
                            );
                            None
                        }
                    };

                    // Decision 5: already-pasted rescue (after readiness+buffer)
                    let first_msg = &undelivered[0];
                    if use_cases::should_defer_visible_fresh_inbox_message(
                        first_msg,
                        chrono::Utc::now(),
                        visibility,
                    ) {
                        tracing::debug!(
                            target_name = &target,
                            session = &session_name,
                            seq = first_msg.seq,
                            "target window visible and inbox message is fresh; deferring delivery"
                        );
                        results.push(InboxPollResult {
                            target,
                            delivered: 0,
                            errors: vec![],
                        });
                        continue;
                    }

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
                        if use_cases::should_defer_visible_fresh_inbox_message(
                            msg,
                            chrono::Utc::now(),
                            visibility,
                        ) {
                            tracing::debug!(
                                target_name = &target,
                                session = &session_name,
                                seq = msg.seq,
                                "target window visible and inbox message is fresh; deferring delivery"
                            );
                            break 'msg_loop;
                        }

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

    /// Drain any completed background archive jobs. The worker reports the
    /// task id and either `Ok(saved)` or an error string. On success we clear
    /// the in-progress entry so subsequent refreshes treat the task as
    /// archived; on failure we still clear the entry (so the task can re-
    /// appear from disk if the worker only partially completed), surface the
    /// error to the status line, and log it.
    fn apply_archive_results(&mut self) {
        loop {
            let (task_id, result) = match self.archive_rx.try_recv() {
                Ok(r) => r,
                Err(_) => return,
            };
            self.archive_in_progress.remove(&task_id);
            match result {
                Ok(saved) => {
                    let label = if saved {
                        "Archived & saved"
                    } else {
                        "Archived"
                    };
                    tracing::info!(task_id = %task_id, saved, "TUI: archive task completed");
                    self.log_output(format!("  Archived task {task_id}"));
                    self.set_status(format!("{label}: {task_id}"));
                }
                Err(e) => {
                    tracing::error!(task_id = %task_id, error = %e, "TUI: archive task failed");
                    self.log_output(format!("  Archive failed for {task_id}: {e}"));
                    self.set_status(format!("archive failed: {task_id}: {e}"));
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

        // Auto-dismiss CI/workagent failure notifications
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
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_caffeinate();
    }
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
        let refresh_interval = Duration::from_secs(1);

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

            // Periodic refresh (drives visible project data and agent activity)
            if last_refresh.elapsed() >= refresh_interval {
                if app.view == View::ProjectList {
                    app.start_project_refresh();
                } else if app.view == View::TaskList {
                    app.refresh_tasks_for_project();
                    app.refresh_agents();
                }
                last_refresh = Instant::now();
            }
            app.apply_project_refresh_result();

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

            // Poll agent inboxes every 2 seconds (deliver messages via tmux send-keys)
            if app.last_inbox_poll.elapsed() >= Duration::from_secs(2) {
                app.start_inbox_poll();
                app.last_inbox_poll = Instant::now();
            }

            // Check for completed inbox poll results (non-blocking)
            app.apply_inbox_poll_results();

            // Check for completed respawn results (non-blocking)
            app.apply_respawn_results();

            // Check for completed task archive results (non-blocking)
            app.apply_archive_results();

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

#[cfg(test)]
mod tests {
    use super::*;
    use agman::agent_model::AgentAttachment;
    use agman::task::{LinkedPr, TaskMeta};

    #[test]
    fn apply_project_refresh_snapshot_preserves_counts_and_clamps_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        Project::create(&config, "alpha", "Alpha project").unwrap();
        Project::create(&config, "beta", "Beta project").unwrap();
        let projects = use_cases::list_projects(&config).unwrap();
        let mut app = App::new_for_test(config).unwrap();
        app.selected_project_index = 99;

        let snapshot = ProjectRefreshSnapshot {
            projects,
            project_task_counts: HashMap::from([("alpha".to_string(), 3)]),
            project_agent_counts: HashMap::from([("alpha".to_string(), 2)]),
            project_active_agent_counts: HashMap::from([("alpha".to_string(), 1)]),
            unassigned_task_count: 1,
            agent_activity: HashMap::from([(
                "agent-session".to_string(),
                AgentActivitySample {
                    last_tmux_activity_epoch: Some(100),
                    last_observed_work_at: Some(Instant::now()),
                    foreground_command: "nvim".to_string(),
                    pane_dead: false,
                    query_ok: true,
                },
            )]),
            project_list_error: None,
            agent_list_error: None,
            agent_activity_error: None,
        };

        app.apply_project_refresh_snapshot(snapshot);

        assert_eq!(app.selected_project_index, app.project_list_len() - 1);
        assert_eq!(app.project_task_counts.get("alpha"), Some(&3));
        assert_eq!(app.project_agent_counts.get("alpha"), Some(&2));
        assert_eq!(app.project_active_agent_counts.get("alpha"), Some(&1));
        assert_eq!(app.unassigned_task_count, 1);
        assert!(app.agent_activity.contains_key("agent-session"));
    }

    #[test]
    fn apply_project_refresh_snapshot_keeps_empty_selection_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let mut app = App::new_for_test(config).unwrap();
        app.selected_project_index = 7;

        app.apply_project_refresh_snapshot(ProjectRefreshSnapshot {
            projects: Vec::new(),
            project_task_counts: HashMap::new(),
            project_agent_counts: HashMap::new(),
            project_active_agent_counts: HashMap::new(),
            unassigned_task_count: 0,
            agent_activity: HashMap::new(),
            project_list_error: None,
            agent_list_error: None,
            agent_activity_error: None,
        });

        assert_eq!(app.project_list_len(), 0);
        assert_eq!(app.selected_project_index, 7);
    }

    #[test]
    fn stale_project_refresh_result_does_not_overwrite_current_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        Project::create(&config, "alpha", "Alpha project").unwrap();
        let projects = use_cases::list_projects(&config).unwrap();
        let mut app = App::new_for_test(config).unwrap();
        app.project_refresh_generation = 2;
        app.project_refresh_active = true;
        app.project_task_counts = HashMap::from([("fresh".to_string(), 1)]);
        app.unassigned_task_count = 4;

        let stale_snapshot = ProjectRefreshSnapshot {
            projects,
            project_task_counts: HashMap::from([("stale".to_string(), 99)]),
            project_agent_counts: HashMap::new(),
            project_active_agent_counts: HashMap::new(),
            unassigned_task_count: 99,
            agent_activity: HashMap::new(),
            project_list_error: None,
            agent_list_error: None,
            agent_activity_error: None,
        };
        app.project_refresh_tx.send((1, stale_snapshot)).unwrap();

        app.apply_project_refresh_result();

        assert!(!app.project_refresh_active);
        assert_eq!(
            app.project_task_counts,
            HashMap::from([("fresh".to_string(), 1)])
        );
        assert_eq!(app.unassigned_task_count, 4);
    }

    #[test]
    fn project_list_opens_global_notes_and_returns_to_project_list() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        Project::create(&config, "alpha", "Alpha project").unwrap();
        Project::create(&config, "beta", "Beta project").unwrap();
        let mut app = App::new_for_test(config.clone()).unwrap();
        app.refresh_projects();
        app.selected_project_index = app
            .projects
            .iter()
            .position(|project| project.meta.name == "beta")
            .unwrap();

        app.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(app.view, View::Notes);
        assert_eq!(app.notes_return_view, View::ProjectList);
        assert_eq!(app.notes_view.as_ref().unwrap().root_dir, config.notes_dir);

        app.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(app.view, View::ProjectList);
        assert!(app.notes_view.is_none());
    }

    #[test]
    fn project_list_opens_global_notes_for_unassigned() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        Project::create(&config, "alpha", "Alpha project").unwrap();
        let mut app = App::new_for_test(config.clone()).unwrap();
        app.refresh_projects();
        app.unassigned_task_count = 1;
        app.selected_project_index = app.projects.len();

        app.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(app.view, View::Notes);
        assert_eq!(app.notes_return_view, View::ProjectList);
        assert_eq!(app.notes_view.as_ref().unwrap().root_dir, config.notes_dir);
    }

    #[test]
    fn task_list_lowercase_o_opens_project_notes_and_returns_to_task_list() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        Project::create(&config, "alpha", "Alpha project").unwrap();
        let mut app = App::new_for_test(config.clone()).unwrap();
        app.current_project = Some("alpha".to_string());
        app.view = View::TaskList;

        app.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(app.view, View::Notes);
        assert_eq!(app.notes_return_view, View::TaskList);
        assert_eq!(
            app.notes_view.as_ref().unwrap().root_dir,
            config.project_notes_dir("alpha")
        );

        app.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(app.view, View::TaskList);
        assert!(app.notes_view.is_none());
    }

    #[test]
    fn task_list_lowercase_o_opens_project_notes_even_when_task_has_linked_pr() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        let mut task = create_test_task(&config, &project, &branch);
        task.meta.linked_pr = Some(LinkedPr {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            owned: true,
            author: None,
        });
        task.save_meta().unwrap();

        let mut app = App::new_for_test(config.clone()).unwrap();
        app.current_project = Some(project.clone());
        app.view = View::TaskList;
        app.refresh_tasks_for_project();

        app.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert_eq!(app.view, View::Notes);
        assert_eq!(app.notes_return_view, View::TaskList);
        assert_eq!(
            app.notes_view.as_ref().unwrap().root_dir,
            config.project_notes_dir(&project)
        );
    }

    #[test]
    fn refresh_tasks_for_project_preserves_attached_agent_row_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        let task = create_test_task(&config, &project, &branch);
        let task_id = task.meta.task_id();
        let researcher = AgentRecord::create(
            &config,
            &project,
            "research-agent-window-names",
            "test researcher",
            AgentKind::Researcher {
                repo: None,
                branch: None,
                task_id: None,
            },
        )
        .unwrap();
        use_cases::attach_agent_to_task(&config, &project, &researcher.meta.name, &task_id, None)
            .unwrap();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project.clone());
        app.refresh_tasks_for_project();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })
                        if agent.meta.name == "research-agent-window-names"
                )
            })
            .expect("attached researcher row exists");

        app.refresh_tasks_for_project();

        assert!(matches!(
            app.selected_project_task_row(),
            Some(ProjectTaskRow::Agent { agent, .. })
                if agent.meta.name == "research-agent-window-names"
        ));
    }

    #[test]
    fn refresh_tasks_for_project_falls_back_to_parent_task_when_agent_row_disappears() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        let task = create_test_task(&config, &project, &branch);
        let task_id = task.meta.task_id();
        let researcher = AgentRecord::create(
            &config,
            &project,
            "research-agent-window-names",
            "test researcher",
            AgentKind::Researcher {
                repo: None,
                branch: None,
                task_id: None,
            },
        )
        .unwrap();
        use_cases::attach_agent_to_task(&config, &project, &researcher.meta.name, &task_id, None)
            .unwrap();

        let mut app = App::new_for_test(config.clone()).unwrap();
        app.current_project = Some(project.clone());
        app.refresh_tasks_for_project();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })
                        if agent.meta.name == "research-agent-window-names"
                )
            })
            .expect("attached researcher row exists");

        use_cases::detach_agent_from_task(&config, &project, &researcher.meta.name).unwrap();
        app.refresh_tasks_for_project();

        assert!(matches!(
            app.selected_project_task_row(),
            Some(ProjectTaskRow::Task { task, .. }) if task.meta.task_id() == task_id
        ));
    }

    #[test]
    fn refresh_tasks_preserves_attached_agent_row_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        let task = create_test_task(&config, &project, &branch);
        let task_id = task.meta.task_id();
        let reviewer = AgentRecord::create(
            &config,
            &project,
            "reviewer-pr5590-ltv",
            "test reviewer",
            AgentKind::Reviewer {
                worktrees: Vec::new(),
            },
        )
        .unwrap();
        use_cases::attach_agent_to_task(&config, &project, &reviewer.meta.name, &task_id, None)
            .unwrap();

        let mut app = App::new_for_test(config).unwrap();
        app.refresh_tasks();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })
                        if agent.meta.name == "reviewer-pr5590-ltv"
                )
            })
            .expect("attached reviewer row exists");

        app.refresh_tasks();

        assert!(matches!(
            app.selected_project_task_row(),
            Some(ProjectTaskRow::Agent { agent, .. }) if agent.meta.name == "reviewer-pr5590-ltv"
        ));
    }

    #[test]
    fn project_detail_rows_order_agents_before_tasks_with_attached_agents_under_task() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        create_test_task(&config, &project, &branch);
        create_test_task(&config, &project, &format!("branch-b-{unique}"));
        AgentRecord::create(
            &config,
            &project,
            "unattached-reviewer",
            "test reviewer",
            AgentKind::Reviewer {
                worktrees: Vec::new(),
            },
        )
        .unwrap();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.selected_index = 0;
        app.clamp_project_detail_selection();

        let rows = app.project_detail_rows();
        assert!(matches!(rows[0], ProjectDetailRow::AgentsSectionHeader));
        assert!(matches!(rows[1], ProjectDetailRow::SectionColumnSpacer));
        assert!(matches!(rows[2], ProjectDetailRow::AgentsColumnsHeader));
        assert!(matches!(
            rows[3],
            ProjectDetailRow::UnattachedAgent { agent }
                if agent.meta.name == "unattached-reviewer"
        ));
        assert!(matches!(rows[4], ProjectDetailRow::SectionSpacer));
        assert!(matches!(rows[5], ProjectDetailRow::TasksSectionHeader));
        assert!(matches!(rows[6], ProjectDetailRow::SectionColumnSpacer));
        assert!(matches!(rows[7], ProjectDetailRow::TasksColumnsHeader));
        assert!(matches!(
            rows[8],
            ProjectDetailRow::Task(ProjectTaskRow::Task { .. })
        ));
        assert!(matches!(rows[9], ProjectDetailRow::AttachedAgentsHeader));
        assert!(matches!(
            rows[10],
            ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })
                if agent.meta.name.starts_with("engineer-")
        ));
        assert!(matches!(rows[11], ProjectDetailRow::TaskGroupSpacer));
        assert!(matches!(
            rows[12],
            ProjectDetailRow::Task(ProjectTaskRow::Task { .. })
        ));
        assert!(matches!(rows[13], ProjectDetailRow::AttachedAgentsHeader));
        assert!(matches!(
            rows[14],
            ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { agent, .. })
                if agent.meta.name.starts_with("engineer-")
        ));
        assert!(!matches!(
            rows.last(),
            Some(ProjectDetailRow::TaskGroupSpacer)
        ));
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::UnattachedAgent { agent })
                if agent.meta.name == "unattached-reviewer"
        ));
    }

    #[test]
    fn project_detail_navigation_skips_headers_and_empty_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        create_test_task(&config, &project, &branch);
        create_test_task(&config, &project, &format!("branch-b-{unique}"));
        AgentRecord::create(
            &config,
            &project,
            "unattached-researcher",
            "test researcher",
            AgentKind::Researcher {
                repo: None,
                branch: None,
                task_id: None,
            },
        )
        .unwrap();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.selected_index = 0;
        app.clamp_project_detail_selection();

        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::UnattachedAgent { .. })
        ));
        app.next_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { .. }))
        ));
        app.next_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::AttachedAgent(
                ProjectTaskRow::Agent { .. }
            ))
        ));
        app.next_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { .. }))
        ));
        app.previous_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::AttachedAgent(
                ProjectTaskRow::Agent { .. }
            ))
        ));
        app.select_last_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::AttachedAgent(
                ProjectTaskRow::Agent { .. }
            ))
        ));
        app.next_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::UnattachedAgent { .. })
        ));

        app.jump_to_next_project_detail_section();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { .. }))
        ));
        app.jump_to_previous_project_detail_section();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::UnattachedAgent { .. })
        ));

        let rows = app.project_detail_rows();
        assert!(matches!(rows[0], ProjectDetailRow::AgentsSectionHeader));
        assert!(matches!(rows[1], ProjectDetailRow::SectionColumnSpacer));
        assert!(matches!(rows[2], ProjectDetailRow::AgentsColumnsHeader));
        assert!(matches!(rows[4], ProjectDetailRow::SectionSpacer));
        assert!(matches!(rows[5], ProjectDetailRow::TasksSectionHeader));
        assert!(matches!(rows[6], ProjectDetailRow::SectionColumnSpacer));
        assert!(matches!(rows[7], ProjectDetailRow::TasksColumnsHeader));
        assert!(matches!(
            rows.iter()
                .find(|row| matches!(row, ProjectDetailRow::AttachedAgentsHeader)),
            Some(ProjectDetailRow::AttachedAgentsHeader)
        ));
        assert!(rows
            .iter()
            .any(|row| matches!(row, ProjectDetailRow::TaskGroupSpacer)));
        assert!(!App::project_detail_row_is_actionable(
            &ProjectDetailRow::TaskGroupSpacer
        ));
        assert!(!App::project_detail_row_is_actionable(
            &ProjectDetailRow::SectionColumnSpacer
        ));
    }

    #[test]
    fn project_detail_select_first_moves_from_later_actionable_row() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        create_test_task(&config, &project, &branch);
        AgentRecord::create(
            &config,
            &project,
            "unattached-tester",
            "test tester",
            AgentKind::Tester {
                worktrees: Vec::new(),
                capabilities: Default::default(),
            },
        )
        .unwrap();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.select_last_project_detail_row();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::AttachedAgent(_))
        ));

        app.select_first_project_detail_row();

        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::UnattachedAgent { agent })
                if agent.meta.name == "unattached-tester"
        ));
    }

    #[test]
    fn archive_task_clamps_to_actionable_combined_row_with_empty_agents_section() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let first = create_test_task(&config, &project, &format!("branch-a-{unique}"));
        let second = create_test_task(&config, &project, &format!("branch-b-{unique}"));
        let first_task_id = first.meta.task_id();
        let second_task_id = second.meta.task_id();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. })
                        if task.meta.task_id() == second_task_id
                )
            })
            .unwrap();

        app.archive_task(false).unwrap();

        match app.selected_project_detail_row() {
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. })) => {
                assert_eq!(task.meta.task_id(), first_task_id);
            }
            Some(ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { task_index, .. })) => {
                assert_eq!(app.tasks[task_index].meta.task_id(), first_task_id);
            }
            other => panic!("expected remaining task row, got {other:?}"),
        }
    }

    #[test]
    fn archive_task_clamps_to_remaining_task_rows_when_unattached_agent_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let first = create_test_task(&config, &project, &format!("branch-a-{unique}"));
        let second = create_test_task(&config, &project, &format!("branch-b-{unique}"));
        let first_task_id = first.meta.task_id();
        let second_task_id = second.meta.task_id();
        AgentRecord::create(
            &config,
            &project,
            "unattached-reviewer",
            "test reviewer",
            AgentKind::Reviewer {
                worktrees: Vec::new(),
            },
        )
        .unwrap();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. })
                        if task.meta.task_id() == second_task_id
                )
            })
            .unwrap();

        app.archive_task(false).unwrap();

        match app.selected_project_detail_row() {
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. })) => {
                assert_eq!(task.meta.task_id(), first_task_id);
            }
            Some(ProjectDetailRow::AttachedAgent(ProjectTaskRow::Agent { task_index, .. })) => {
                assert_eq!(app.tasks[task_index].meta.task_id(), first_task_id);
            }
            other => panic!("expected remaining task row, got {other:?}"),
        }
    }

    #[test]
    fn archive_task_marks_in_progress_and_refresh_does_not_re_add_task() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let first = create_test_task(&config, &project, &format!("branch-a-{unique}"));
        let second = create_test_task(&config, &project, &format!("branch-b-{unique}"));
        let first_task_id = first.meta.task_id();
        let second_task_id = second.meta.task_id();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();

        // Directly exercise the live-list filter: the background worker may
        // or may not have finished by the time the periodic refresh fires,
        // so the in-progress set must keep the row suppressed independent of
        // the on-disk `archived_at` field. Insert manually to avoid coupling
        // the assertion to the worker's progress.
        app.archive_in_progress.insert(second_task_id.clone());
        app.tasks.retain(|t| t.meta.task_id() != second_task_id);

        // On-disk meta for `second` still lacks `archived_at` here — without
        // the filter the periodic refresh would resurrect the row.
        app.refresh_tasks_for_project();

        assert!(
            !app.tasks.iter().any(|t| t.meta.task_id() == second_task_id),
            "refresh must not re-add a task whose archive is in progress"
        );
        assert!(
            app.tasks.iter().any(|t| t.meta.task_id() == first_task_id),
            "other tasks remain visible after refresh"
        );

        // Clearing the in-progress flag (as `apply_archive_results` does on
        // completion) lets a subsequent refresh consult disk again. With the
        // on-disk task still un-archived, the row reappears — confirming
        // the filter, not some unrelated path, was what suppressed it.
        app.archive_in_progress.remove(&second_task_id);
        app.refresh_tasks_for_project();
        assert!(
            app.tasks.iter().any(|t| t.meta.task_id() == second_task_id),
            "task reappears once in-progress flag is cleared (proves the filter was the gate)"
        );
    }

    #[test]
    fn archive_task_inserts_task_id_into_archive_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let first = create_test_task(&config, &project, &format!("branch-a-{unique}"));
        let second = create_test_task(&config, &project, &format!("branch-b-{unique}"));
        let _first_task_id = first.meta.task_id();
        let second_task_id = second.meta.task_id();

        let mut app = App::new_for_test(config).unwrap();
        app.current_project = Some(project);
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. })
                        if task.meta.task_id() == second_task_id
                )
            })
            .unwrap();

        // `archive_task` returns immediately and spawns the heavy work. We
        // assert only on the synchronous side-effects: the task id is
        // tracked as in-progress and the row is gone from `self.tasks`.
        // The background worker drains independently and is not awaited.
        app.archive_task(false).unwrap();

        assert!(
            app.archive_in_progress.contains(&second_task_id),
            "archived task id should be in archive_in_progress immediately, got {:?}",
            app.archive_in_progress
        );
        assert!(
            !app.tasks.iter().any(|t| t.meta.task_id() == second_task_id),
            "archived task should be removed from in-memory tasks immediately"
        );
    }

    #[test]
    fn refresh_agents_preserves_and_falls_back_from_unattached_agent_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let unique = unique_name();
        let project = format!("repo-{unique}");
        let branch = format!("branch-{unique}");
        let task = create_test_task(&config, &project, &branch);
        let task_id = task.meta.task_id();
        AgentRecord::create(
            &config,
            &project,
            "unattached-operator",
            "test operator",
            AgentKind::Operator {
                repo: None,
                branch: None,
                task_id: None,
            },
        )
        .unwrap();

        let mut app = App::new_for_test(config.clone()).unwrap();
        app.current_project = Some(project.clone());
        app.refresh_tasks_for_project();
        app.refresh_agents();
        app.selected_index = app
            .project_detail_rows()
            .iter()
            .position(|row| {
                matches!(
                    row,
                    ProjectDetailRow::UnattachedAgent { agent }
                        if agent.meta.name == "unattached-operator"
                )
            })
            .unwrap();

        app.refresh_agents();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::UnattachedAgent { agent })
                if agent.meta.name == "unattached-operator"
        ));

        use_cases::archive_agent(&config, &project, "unattached-operator").unwrap();
        app.refresh_agents();
        assert!(matches!(
            app.selected_project_detail_row(),
            Some(ProjectDetailRow::Task(ProjectTaskRow::Task { task, .. }))
                if task.meta.task_id() == task_id
        ));
    }

    fn test_config(root: &Path) -> Config {
        Config::new(root.join(".agman"), root.join("repos"))
    }

    fn create_test_task(config: &Config, repo_name: &str, branch_name: &str) -> Task {
        config.ensure_dirs().unwrap();

        let worktree_path = config.worktree_path(repo_name, branch_name);
        std::fs::create_dir_all(&worktree_path).unwrap();

        let dir = config.task_dir(repo_name, branch_name);
        std::fs::create_dir_all(&dir).unwrap();

        let meta = TaskMeta::new(
            repo_name.to_string(),
            branch_name.to_string(),
            worktree_path,
            "new".to_string(),
        );
        let mut task = Task { meta, dir };
        task.meta.project = Some(repo_name.to_string());
        task.save_meta().unwrap();

        let task_id = task.meta.task_id();
        let engineer_name = format!("engineer-{}", task_id.replace("--", "-"));
        AgentRecord::create_with_attachment(
            config,
            repo_name,
            &engineer_name,
            &format!("Engineer attached to task {task_id}"),
            AgentKind::Engineer,
            AgentAttachment::Task {
                task_id,
                role_label: Some("Engineer".to_string()),
            },
        )
        .unwrap();

        task
    }

    fn unique_name() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{}-{nanos}", std::process::id())
    }
}
