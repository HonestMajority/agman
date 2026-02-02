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
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let tasks = Task::list_all(&config)?;
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
        })
    }

    fn create_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_cursor_line_style(ratatui::style::Style::default());
        editor
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

    pub fn selected_task_mut(&mut self) -> Option<&mut Task> {
        self.tasks.get_mut(self.selected_index)
    }

    pub fn set_status(&mut self, message: String) {
        self.status_message = Some((message, Instant::now()));
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
        self.preview_scroll = 0;
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

    fn pause_task(&mut self) -> Result<()> {
        let task_id = self.selected_task().map(|t| t.meta.task_id());
        if let Some(task) = self.selected_task_mut() {
            task.update_status(TaskStatus::Paused)?;
        }
        if let Some(id) = task_id {
            self.set_status(format!("Paused: {}", id));
        }
        Ok(())
    }

    fn resume_task(&mut self) -> Result<()> {
        let task_id = self.selected_task().map(|t| t.meta.task_id());
        if let Some(task) = self.selected_task_mut() {
            task.update_status(TaskStatus::Working)?;
        }
        if let Some(id) = task_id {
            self.set_status(format!("Resumed: {}", id));
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

        // Kill tmux session
        let _ = Tmux::kill_session(&tmux_session);

        // Remove worktree
        let repo_path = self.config.repo_path(&repo_name);
        let _ = Git::remove_worktree(&repo_path, &worktree_path);

        // Delete branch
        let _ = Git::delete_branch(&repo_path, &branch_name);

        // Delete task directory
        task.delete(&self.config)?;

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
            task.meta.task_id()
        } else {
            self.view = View::Preview;
            return Ok(());
        };

        // Run agman continue in the background
        let status = Command::new("agman")
            .args(["continue", &task_id, &feedback, "--flow", "continue"])
            .status();

        match status {
            Ok(s) if s.success() => {
                self.set_status(format!("Feedback submitted, flow started for {}", task_id));
                self.refresh_tasks()?;
            }
            Ok(_) => {
                self.set_status("Failed to start continue flow".to_string());
            }
            Err(e) => {
                self.set_status(format!("Error: {}", e));
            }
        }

        self.view = View::Preview;
        self.load_preview();
        Ok(())
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool> {
        self.clear_old_status();

        match self.view {
            View::TaskList => self.handle_task_list_event(event),
            View::Preview => self.handle_preview_event(event),
            View::DeleteConfirm => self.handle_delete_confirm_event(event),
            View::Feedback => self.handle_feedback_event(event),
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
                KeyCode::Char('p') => {
                    self.pause_task()?;
                }
                KeyCode::Char('r') => {
                    self.resume_task()?;
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
                }
            }
        }
        Ok(false)
    }

    fn handle_feedback_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Esc => {
                    // Cancel feedback
                    self.view = View::Preview;
                    self.set_status("Feedback cancelled".to_string());
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // Submit feedback with Ctrl+Enter
                    self.submit_feedback()?;
                }
                _ => {
                    let input = Input::from(event.clone());
                    self.feedback_editor.input(input);
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
}

pub fn run_tui(config: Config) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new(config)?;

    // Main loop
    let mut attach_session: Option<String> = None;

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

        // Periodic refresh of task status
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

    // Attach to tmux if requested
    if let Some(session) = attach_session {
        Tmux::attach_session(&session)?;
    }

    Ok(())
}
