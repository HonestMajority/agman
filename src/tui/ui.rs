use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::task::TaskStatus;

use super::app::{App, View};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(f.area());

    match app.view {
        View::TaskList => draw_task_list(f, app, chunks[0]),
        View::Preview => draw_preview(f, app, chunks[0]),
        View::Notes => draw_notes(f, app, chunks[0]),
        View::DeleteConfirm => {
            draw_task_list(f, app, chunks[0]);
            draw_delete_confirm(f, app);
        }
    }

    draw_status_bar(f, app, chunks[1]);
}

fn draw_task_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let status_icon = match task.meta.status {
                TaskStatus::Working => ("●", Color::Green),
                TaskStatus::Paused => ("◐", Color::Yellow),
                TaskStatus::Done => ("✓", Color::Cyan),
                TaskStatus::Failed => ("✗", Color::Red),
            };

            let agent_str = task
                .meta
                .current_agent
                .as_deref()
                .unwrap_or("");

            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(status_icon.0, Style::default().fg(status_icon.1)),
                Span::raw(" "),
                Span::styled(
                    format!("{:<20}", task.meta.branch_name),
                    if i == app.selected_index {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("{:<10}", task.meta.status),
                    Style::default().fg(status_icon.1),
                ),
                Span::styled(
                    format!("{:<12}", agent_str),
                    Style::default().fg(Color::Blue),
                ),
                Span::styled(
                    task.time_since_update(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);

            let style = if i == app.selected_index {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" agman ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );

    f.render_widget(list, area);
}

fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    // Task info header
    if let Some(task) = app.selected_task() {
        let header = Paragraph::new(format!(
            "Task: {} | Status: {} | Agent: {}",
            task.meta.branch_name,
            task.meta.status,
            task.meta.current_agent.as_deref().unwrap_or("none")
        ))
        .block(
            Block::default()
                .title(" Task Info ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        f.render_widget(header, chunks[0]);
    }

    // Log preview
    let preview = Paragraph::new(app.preview_content.as_str())
        .block(
            Block::default()
                .title(" agent.log (press Enter to attach, Esc to go back) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.preview_scroll, 0));

    f.render_widget(preview, chunks[1]);
}

fn draw_notes(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    // Task info header
    if let Some(task) = app.selected_task() {
        let header = Paragraph::new(format!("Notes for: {}", task.meta.branch_name))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(header, chunks[0]);
    }

    // Notes editor
    app.notes_editor.set_block(
        Block::default()
            .title(" Notes (Esc to save & exit, Ctrl+S to save) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );

    f.render_widget(&app.notes_editor, chunks[1]);
}

fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 20, f.area());

    f.render_widget(Clear, area);

    let task_name = app
        .selected_task()
        .map(|t| t.meta.branch_name.as_str())
        .unwrap_or("unknown");

    let text = format!(
        "Delete task '{}'?\n\nThis will:\n- Kill the tmux session\n- Remove the git worktree\n- Delete all task files\n\n[y] Yes  [n] No",
        task_name
    );

    let popup = Paragraph::new(text)
        .block(
            Block::default()
                .title(" Confirm Delete ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(popup, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.view {
        View::TaskList => "[↑↓] navigate  [Enter] preview  [n] notes  [p] pause  [r] resume  [d] delete  [R] refresh  [q] quit",
        View::Preview => "[↑↓] scroll  [Enter] attach tmux  [Esc] back",
        View::Notes => "[Esc] save & exit  [Ctrl+S] save",
        View::DeleteConfirm => "[y] confirm  [n] cancel",
    };

    let status_text = app
        .status_message
        .as_ref()
        .map(|(msg, _)| format!(" | {}", msg))
        .unwrap_or_default();

    let status = Paragraph::new(format!("{}{}", help_text, status_text))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

    f.render_widget(status, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
