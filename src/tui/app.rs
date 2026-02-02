use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::time::{Duration, Instant};
use tui_textarea::TextArea;

use crate::config::Config;
use crate::git::Git;
use crate::task::{Task, TaskStatus};
use crate::tmux::Tmux;

use super::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    TaskList,
    Preview,
    Notes,
    DeleteConfirm,
}

pub struct App {
    pub config: Config,
    pub tasks: Vec<Task>,
    pub selected_index: usize,
    pub view: View,
    pub preview_content: String,
    pub preview_scroll: u16,
    pub notes_editor: TextArea<'static>,
    pub should_quit: bool,
    pub status_message: Option<(String, Instant)>,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let tasks = Task::list_all(&config)?;
        let mut notes_editor = TextArea::default();
        notes_editor.set_cursor_line_style(ratatui::style::Style::default());

        Ok(Self {
            config,
            tasks,
            selected_index: 0,
            view: View::TaskList,
            preview_content: String::new(),
            preview_scroll: 0,
            notes_editor,
            should_quit: false,
            status_message: None,
        })
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
        if let Some(task) = self.selected_task() {
            self.preview_content = task
                .read_agent_log_tail(50)
                .unwrap_or_else(|_| "No agent log available".to_string());
        }
    }

    fn load_notes(&mut self) {
        if let Some(task) = self.selected_task() {
            let notes = task.read_notes().unwrap_or_default();
            self.notes_editor = TextArea::from(notes.lines());
            self.notes_editor
                .set_cursor_line_style(ratatui::style::Style::default());
        }
    }

    fn save_notes(&mut self) -> Result<()> {
        if let Some(task) = self.selected_task() {
            let notes = self.notes_editor.lines().join("\n");
            task.write_notes(&notes)?;
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

    #[allow(dead_code)]
    fn attach_to_task(&self) -> Result<()> {
        if let Some(task) = self.selected_task() {
            if Tmux::session_exists(&task.meta.tmux_session) {
                // We need to exit the TUI first, then attach
                // This will be handled in the main loop
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool> {
        self.clear_old_status();

        match self.view {
            View::TaskList => self.handle_task_list_event(event),
            View::Preview => self.handle_preview_event(event),
            View::Notes => self.handle_notes_event(event),
            View::DeleteConfirm => self.handle_delete_confirm_event(event),
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
                KeyCode::Up | KeyCode::Char('k') => {
                    self.previous_task();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.next_task();
                }
                KeyCode::Enter => {
                    self.load_preview();
                    self.view = View::Preview;
                }
                KeyCode::Char('n') => {
                    self.load_notes();
                    self.view = View::Notes;
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
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_preview_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.view = View::TaskList;
                }
                KeyCode::Enter => {
                    // Attach to tmux - return true to signal we need to exit and attach
                    if let Some(task) = self.selected_task() {
                        if Tmux::session_exists(&task.meta.tmux_session) {
                            return Ok(true);
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.preview_scroll = self.preview_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.preview_scroll = self.preview_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    self.preview_scroll = self.preview_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    self.preview_scroll = self.preview_scroll.saturating_add(10);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn handle_notes_event(&mut self, event: Event) -> Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Esc => {
                    self.save_notes()?;
                    self.view = View::TaskList;
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.save_notes()?;
                }
                _ => {
                    self.notes_editor.input(event);
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
