use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::task::TaskStatus;

use super::app::{App, PreviewPane, View};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(3)])
        .split(f.area());

    match app.view {
        View::TaskList => draw_task_list(f, app, chunks[0]),
        View::Preview => draw_preview(f, app, chunks[0]),
        View::DeleteConfirm => {
            draw_task_list(f, app, chunks[0]);
            draw_delete_confirm(f, app);
        }
        View::Feedback => {
            draw_preview(f, app, chunks[0]);
            draw_feedback(f, app);
        }
    }

    draw_status_bar(f, app, chunks[1]);
}

fn draw_task_list(f: &mut Frame, app: &App, area: Rect) {
    // Create the outer block first
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(
                " agman ",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({} tasks) ", app.tasks.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightCyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split inner area into header and list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    // Render header
    let header = Line::from(vec![
        Span::raw("     "),
        Span::styled(
            format!("{:<32}", "TASK"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<10}", "STATUS"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<14}", "AGENT"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "UPDATED",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    // Render task list
    let items: Vec<ListItem> = app
        .tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let (status_icon, status_color) = match task.meta.status {
                TaskStatus::Working => ("●", Color::LightGreen),
                TaskStatus::Paused => ("◐", Color::Yellow),
                TaskStatus::Done => ("✓", Color::LightCyan),
                TaskStatus::Failed => ("✗", Color::LightRed),
            };

            let agent_str = task.meta.current_agent.as_deref().unwrap_or("-");
            let task_id = task.meta.task_id();

            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(status_icon, Style::default().fg(status_color)),
                Span::raw("  "),
                Span::styled(
                    format!("{:<32}", task_id),
                    if i == app.selected_index {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(
                    format!("{:<10}", task.meta.status),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!("{:<14}", agent_str),
                    Style::default().fg(Color::LightBlue),
                ),
                Span::styled(task.time_since_update(), Style::default().fg(Color::DarkGray)),
            ]);

            let style = if i == app.selected_index {
                Style::default().bg(Color::Rgb(40, 40, 50))
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, chunks[1]);
}

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Task info header
    if let Some(task) = app.selected_task() {
        let header = Paragraph::new(Line::from(vec![
            Span::styled("Task: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                task.meta.task_id(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", task.meta.status),
                Style::default().fg(match task.meta.status {
                    TaskStatus::Working => Color::LightGreen,
                    TaskStatus::Paused => Color::Yellow,
                    TaskStatus::Done => Color::LightCyan,
                    TaskStatus::Failed => Color::LightRed,
                }),
            ),
            Span::raw("  "),
            Span::styled("Agent: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                task.meta.current_agent.as_deref().unwrap_or("none"),
                Style::default().fg(Color::LightBlue),
            ),
        ]))
        .block(
            Block::default()
                .title(Span::styled(
                    " Task Info ",
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightCyan)),
        );
        f.render_widget(header, chunks[0]);
    }

    // Split the remaining area into logs and notes panels
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[1]);

    draw_logs_panel(f, app, panels[0]);
    draw_notes_panel(f, app, panels[1]);
}

fn draw_logs_panel(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.preview_pane == PreviewPane::Logs;
    let border_color = if is_focused {
        Color::LightYellow
    } else {
        Color::DarkGray
    };

    let title_style = if is_focused {
        Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let logs = Paragraph::new(app.preview_content.as_str())
        .block(
            Block::default()
                .title(Span::styled(" Logs (Enter: attach tmux) ", title_style))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .style(Style::default().fg(Color::Gray))
        .wrap(Wrap { trim: false })
        .scroll((app.preview_scroll, 0));

    f.render_widget(logs, area);
}

fn draw_notes_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.preview_pane == PreviewPane::Notes;
    let border_color = if is_focused {
        Color::LightGreen
    } else {
        Color::DarkGray
    };

    let title = if app.notes_editing {
        " Notes [EDITING] "
    } else if is_focused {
        " Notes (i: edit, Enter: edit) "
    } else {
        " Notes "
    };

    let title_style = if is_focused {
        Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    if app.notes_editing {
        // Show the editor
        app.notes_editor.set_block(
            Block::default()
                .title(Span::styled(title, title_style))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightGreen)),
        );
        app.notes_editor
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
        f.render_widget(&app.notes_editor, area);
    } else {
        // Show read-only notes
        let notes = Paragraph::new(app.notes_content.as_str())
            .block(
                Block::default()
                    .title(Span::styled(title, title_style))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color)),
            )
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: false })
            .scroll((app.notes_scroll, 0));

        f.render_widget(notes, area);
    }
}

fn draw_feedback(f: &mut Frame, app: &mut App) {
    let area = centered_rect(70, 50, f.area());

    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Feedback for: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            task_id,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(
        Block::default()
            .title(Span::styled(
                " Continue Task ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightMagenta)),
    );
    f.render_widget(header, chunks[0]);

    // Editor
    app.feedback_editor.set_block(
        Block::default()
            .title(Span::styled(
                " Enter feedback (Ctrl+Enter to submit, Esc to cancel) ",
                Style::default().fg(Color::LightGreen),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightGreen)),
    );
    app.feedback_editor
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&app.feedback_editor, chunks[1]);
}

fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 30, f.area());

    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Delete task '{}'?", task_id),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("This will:", Style::default().fg(Color::Gray))),
        Line::from(Span::styled(
            "  - Kill the tmux session",
            Style::default().fg(Color::LightRed),
        )),
        Line::from(Span::styled(
            "  - Remove the git worktree",
            Style::default().fg(Color::LightRed),
        )),
        Line::from(Span::styled(
            "  - Delete all task files",
            Style::default().fg(Color::LightRed),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y] ", Style::default().fg(Color::LightGreen)),
            Span::styled("Yes", Style::default().fg(Color::White)),
            Span::raw("    "),
            Span::styled("[n] ", Style::default().fg(Color::LightRed)),
            Span::styled("No", Style::default().fg(Color::White)),
        ]),
    ];

    let popup = Paragraph::new(text).block(
        Block::default()
            .title(Span::styled(
                " Confirm Delete ",
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightRed)),
    );

    f.render_widget(popup, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.view {
        View::TaskList => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("l", Style::default().fg(Color::LightCyan)),
                Span::styled(" preview  ", Style::default().fg(Color::DarkGray)),
                Span::styled("f", Style::default().fg(Color::LightMagenta)),
                Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                Span::styled("p", Style::default().fg(Color::LightCyan)),
                Span::styled(" pause  ", Style::default().fg(Color::DarkGray)),
                Span::styled("r", Style::default().fg(Color::LightCyan)),
                Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
                Span::styled("d", Style::default().fg(Color::LightCyan)),
                Span::styled(" delete  ", Style::default().fg(Color::DarkGray)),
                Span::styled("q", Style::default().fg(Color::LightCyan)),
                Span::styled(" quit", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::Preview => {
            if app.notes_editing {
                vec![
                    Span::styled("Esc", Style::default().fg(Color::LightGreen)),
                    Span::styled(" save & exit editing", Style::default().fg(Color::DarkGray)),
                ]
            } else {
                vec![
                    Span::styled("Ctrl+h/l", Style::default().fg(Color::LightCyan)),
                    Span::styled(" pane  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("f", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("i", Style::default().fg(Color::LightCyan)),
                    Span::styled(" edit notes  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::LightCyan)),
                    Span::styled(" attach  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("h", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]
            }
        }
        View::DeleteConfirm => {
            vec![
                Span::styled("y", Style::default().fg(Color::LightGreen)),
                Span::styled(" confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled("n/Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::Feedback => {
            vec![
                Span::styled("Ctrl+Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" submit  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
    };

    let mut line_spans = help_text;

    if let Some((msg, _)) = &app.status_message {
        line_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        line_spans.push(Span::styled(msg, Style::default().fg(Color::LightYellow)));
    }

    let status = Paragraph::new(Line::from(line_spans)).block(
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
