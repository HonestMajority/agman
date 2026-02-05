use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::task::TaskStatus;

use super::app::{App, BranchMode, PreviewPane, View, WizardStep};
use super::vim::VimMode;

pub fn draw(f: &mut Frame, app: &mut App) {
    // Check if we're showing a modal that should hide the output pane
    let is_modal_view = matches!(
        app.view,
        View::DeleteConfirm | View::Feedback | View::NewTaskWizard | View::CommandList | View::TaskEditor
    );

    // Determine output pane height based on content (hide during modals)
    let output_height = if app.output_log.is_empty() || is_modal_view {
        0
    } else {
        (app.output_log.len() as u16 + 2).min(8) // 2 for borders, max 8 lines
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(output_height),
            Constraint::Length(3),
        ])
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
        View::NewTaskWizard => {
            draw_task_list(f, app, chunks[0]);
            draw_wizard(f, app);
        }
        View::CommandList => {
            draw_task_list(f, app, chunks[0]);
            draw_command_list(f, app);
        }
        View::TaskEditor => {
            draw_preview(f, app, chunks[0]);
            draw_task_editor(f, app);
        }
    }

    if output_height > 0 {
        draw_output_pane(f, app, chunks[1]);
    }

    draw_status_bar(f, app, chunks[2]);
}

fn draw_task_list(f: &mut Frame, app: &App, area: Rect) {
    // Count running and stopped tasks
    let running_count = app.tasks.iter().filter(|t| t.meta.status == TaskStatus::Running).count();
    let stopped_count = app.tasks.len() - running_count;

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

    // Calculate dynamic task column width
    // Fixed columns: icon(1) + spaces(3) + status(10) + agent(12) + updated(~10) + gaps(9) = ~45
    const FIXED_COLS_WIDTH: u16 = 48;
    const MIN_TASK_WIDTH: usize = 15;
    const STATUS_WIDTH: usize = 10;
    const AGENT_WIDTH: usize = 12;
    const COL_GAP: &str = "   "; // 3 spaces between columns

    let available_width = inner.width.saturating_sub(FIXED_COLS_WIDTH) as usize;

    // Find the longest task ID
    let max_task_len = app
        .tasks
        .iter()
        .map(|t| t.meta.task_id().len())
        .max()
        .unwrap_or(MIN_TASK_WIDTH);

    // Task column width: use max task length, but cap it to available space
    let task_width = max_task_len.max(MIN_TASK_WIDTH).min(available_width.max(MIN_TASK_WIDTH));

    // Render header - columns: icon(1) + space(2) + task(dynamic) + gap + status + gap + agent + gap + updated
    let header = Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<width$}", "TASK", width = task_width),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(COL_GAP),
        Span::styled(
            format!("{:<width$}", "STATUS", width = STATUS_WIDTH),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(COL_GAP),
        Span::styled(
            format!("{:<width$}", "AGENT", width = AGENT_WIDTH),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(COL_GAP),
        Span::styled(
            "UPDATED",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    // Build task list (sorted by updated_at, running tasks first)
    let mut items: Vec<ListItem> = Vec::new();
    let mut task_index = 0;
    let mut shown_running_header = false;
    let mut shown_stopped_header = false;

    for task in &app.tasks {
        let is_running = task.meta.status == TaskStatus::Running;

        // Add section header if needed
        if is_running && !shown_running_header && running_count > 0 {
            let header_line = Line::from(vec![
                Span::styled(
                    format!("── Running ({}) ", running_count),
                    Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "─".repeat(50),
                    Style::default().fg(Color::Rgb(60, 60, 60)),
                ),
            ]);
            items.push(ListItem::new(header_line));
            shown_running_header = true;
        } else if !is_running && !shown_stopped_header && stopped_count > 0 {
            // Add spacing before stopped section if there were running tasks
            if shown_running_header {
                items.push(ListItem::new(Line::from("")));
            }
            let header_line = Line::from(vec![
                Span::styled(
                    format!("── Stopped ({}) ", stopped_count),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "─".repeat(48),
                    Style::default().fg(Color::Rgb(40, 40, 40)),
                ),
            ]);
            items.push(ListItem::new(header_line));
            shown_stopped_header = true;
        }

        // Render the task
        let (status_icon, status_color) = match task.meta.status {
            TaskStatus::Running => ("●", Color::LightGreen),
            TaskStatus::Stopped => ("○", Color::DarkGray),
        };

        // Only show agent if task is running
        let agent_str = if is_running {
            task.meta.current_agent.as_deref().unwrap_or("-")
        } else {
            "-"
        };
        let task_id = task.meta.task_id();
        let status_str = format!("{}", task.meta.status);

        // Truncate task_id if needed, with ellipsis
        let display_task_id = if task_id.len() > task_width {
            format!("{}…", &task_id[..task_width.saturating_sub(1)])
        } else {
            task_id.clone()
        };

        // Dim stopped tasks
        let text_color = if is_running {
            if task_index == app.selected_index {
                Color::White
            } else {
                Color::Gray
            }
        } else {
            Color::Rgb(100, 100, 100)
        };

        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(status_icon, Style::default().fg(status_color)),
            Span::raw("  "),
            Span::styled(
                format!("{:<width$}", display_task_id, width = task_width),
                if task_index == app.selected_index {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(text_color)
                },
            ),
            Span::raw(COL_GAP),
            Span::styled(
                format!("{:<width$}", status_str, width = STATUS_WIDTH),
                Style::default().fg(status_color),
            ),
            Span::raw(COL_GAP),
            Span::styled(
                format!("{:<width$}", agent_str, width = AGENT_WIDTH),
                if is_running {
                    Style::default().fg(Color::LightBlue)
                } else {
                    Style::default().fg(Color::Rgb(80, 80, 80))
                },
            ),
            Span::raw(COL_GAP),
            Span::styled(
                task.time_since_update(),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let style = if task_index == app.selected_index {
            Style::default().bg(Color::Rgb(40, 40, 50))
        } else {
            Style::default()
        };

        items.push(ListItem::new(line).style(style));
        task_index += 1;
    }

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
        let is_running = task.meta.status == TaskStatus::Running;
        let agent_str = if is_running {
            task.meta.current_agent.as_deref().unwrap_or("none")
        } else {
            "none"
        };
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
                    TaskStatus::Running => Color::LightGreen,
                    TaskStatus::Stopped => Color::DarkGray,
                }),
            ),
            Span::raw("  "),
            Span::styled("Agent: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                agent_str,
                if is_running {
                    Style::default().fg(Color::LightBlue)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
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

    // Split the remaining area into logs and notes panels (60/40)
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60),
            Constraint::Percentage(40),
        ])
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
        let mode = app.notes_editor.mode();
        format!(" Notes [{}] ", mode.indicator())
    } else if is_focused {
        " Notes (i: edit) ".to_string()
    } else {
        " Notes ".to_string()
    };

    let title_style = if app.notes_editing {
        let mode = app.notes_editor.mode();
        let mode_color = match mode {
            VimMode::Normal => Color::LightCyan,
            VimMode::Insert => Color::LightGreen,
            VimMode::Visual => Color::LightYellow,
            VimMode::Operator(_) => Color::LightMagenta,
        };
        Style::default()
            .fg(mode_color)
            .add_modifier(Modifier::BOLD)
    } else if is_focused {
        Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    if app.notes_editing {
        let mode = app.notes_editor.mode();
        let border_color = match mode {
            VimMode::Normal => Color::LightCyan,
            VimMode::Insert => Color::LightGreen,
            VimMode::Visual => Color::LightYellow,
            VimMode::Operator(_) => Color::LightMagenta,
        };

        // Show the editor
        app.notes_editor.textarea.set_block(
            Block::default()
                .title(Span::styled(title, title_style))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        );
        app.notes_editor.textarea
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
        f.render_widget(&app.notes_editor.textarea, area);
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

fn draw_task_editor(f: &mut Frame, app: &mut App) {
    let area = centered_rect(80, 70, f.area());

    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let mode = app.task_file_editor.mode();
    let mode_color = match mode {
        VimMode::Normal => Color::LightCyan,
        VimMode::Insert => Color::LightGreen,
        VimMode::Visual => Color::LightYellow,
        VimMode::Operator(_) => Color::LightMagenta,
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Editing: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            task_id,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            mode.indicator(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
    ]))
    .block(
        Block::default()
            .title(Span::styled(
                " TASK.md Editor ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightMagenta)),
    );
    f.render_widget(header, chunks[0]);

    // Editor
    app.task_file_editor.textarea.set_block(
        Block::default()
            .title(Span::styled(
                " Ctrl+S to save & close, Esc (in normal) to cancel ",
                Style::default().fg(mode_color),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(mode_color)),
    );
    app.task_file_editor.textarea
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&app.task_file_editor.textarea, chunks[1]);
}

fn draw_feedback(f: &mut Frame, app: &mut App) {
    let area = centered_rect(70, 50, f.area());

    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let mode = app.feedback_editor.mode();
    let mode_color = match mode {
        VimMode::Normal => Color::LightCyan,
        VimMode::Insert => Color::LightGreen,
        VimMode::Visual => Color::LightYellow,
        VimMode::Operator(_) => Color::LightMagenta,
    };

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
        Span::styled("  [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            mode.indicator(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
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
    app.feedback_editor.textarea.set_block(
        Block::default()
            .title(Span::styled(
                " Enter feedback (Ctrl+S to submit, Esc in normal to cancel) ",
                Style::default().fg(mode_color),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(mode_color)),
    );
    app.feedback_editor.textarea
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&app.feedback_editor.textarea, chunks[1]);
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

fn draw_output_pane(f: &mut Frame, app: &App, area: Rect) {
    let content = app.output_log.join("\n");

    let output = Paragraph::new(content)
        .block(
            Block::default()
                .title(Span::styled(
                    " Output ",
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .style(Style::default().fg(Color::Gray))
        .wrap(Wrap { trim: false })
        .scroll((app.output_scroll, 0));

    f.render_widget(output, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.view {
        View::TaskList => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("n", Style::default().fg(Color::LightGreen)),
                Span::styled(" new  ", Style::default().fg(Color::DarkGray)),
                Span::styled("t", Style::default().fg(Color::LightMagenta)),
                Span::styled(" task  ", Style::default().fg(Color::DarkGray)),
                Span::styled("x", Style::default().fg(Color::LightMagenta)),
                Span::styled(" cmd  ", Style::default().fg(Color::DarkGray)),
                Span::styled("f", Style::default().fg(Color::LightMagenta)),
                Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                Span::styled("S", Style::default().fg(Color::LightRed)),
                Span::styled(" stop  ", Style::default().fg(Color::DarkGray)),
                Span::styled("d", Style::default().fg(Color::LightCyan)),
                Span::styled(" del  ", Style::default().fg(Color::DarkGray)),
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
                    Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                    Span::styled(" pane  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("t", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" task  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("f", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("i", Style::default().fg(Color::LightCyan)),
                    Span::styled(" edit  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::LightCyan)),
                    Span::styled(" attach  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]
            }
        }
        View::TaskEditor => {
            vec![
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" save & close  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
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
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" submit  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::NewTaskWizard => {
            if let Some(wizard) = &app.wizard {
                match wizard.step {
                    WizardStep::SelectRepo => {
                        vec![
                            Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                            Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                    WizardStep::SelectBranch => {
                        vec![
                            Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                            Span::styled(" mode  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                            Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" back", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                    WizardStep::EnterDescription => {
                        vec![
                            Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                            Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" back", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                    WizardStep::SelectFlow => {
                        vec![
                            Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                            Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" back", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                }
            } else {
                vec![]
            }
        }
        View::CommandList => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" run  ", Style::default().fg(Color::DarkGray)),
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

fn draw_wizard(f: &mut Frame, app: &mut App) {
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);

    // Extract data we need before mutable borrows
    let (step, step_num, step_title, error_message) = {
        let wizard = match &app.wizard {
            Some(w) => w,
            None => return,
        };
        let (step_num, step_title) = match wizard.step {
            WizardStep::SelectRepo => (1, "Select Repository"),
            WizardStep::SelectBranch => (2, "Branch Name"),
            WizardStep::EnterDescription => (3, "Task Description"),
            WizardStep::SelectFlow => (4, "Select Flow"),
        };
        (wizard.step, step_num, step_title, wizard.error_message.clone())
    };

    // Main wizard container
    let block = Block::default()
        .title(Span::styled(
            format!(" New Task [{}/4] {} ", step_num, step_title),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightCyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split inner area into content and error/help
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(inner);

    // Draw step-specific content
    match step {
        WizardStep::SelectRepo => {
            if let Some(wizard) = &app.wizard {
                draw_wizard_repo_list(f, wizard, chunks[0]);
            }
        }
        WizardStep::SelectBranch => draw_wizard_branch(f, app, chunks[0]),
        WizardStep::EnterDescription => draw_wizard_description(f, app, chunks[0]),
        WizardStep::SelectFlow => {
            if let Some(wizard) = &app.wizard {
                draw_wizard_flow_list(f, wizard, chunks[0]);
            }
        }
    }

    // Draw error message or help text
    draw_wizard_footer_direct(f, step, error_message, chunks[1]);
}

fn draw_wizard_repo_list(f: &mut Frame, wizard: &super::app::NewTaskWizard, area: Rect) {
    let items: Vec<ListItem> = wizard
        .repos
        .iter()
        .enumerate()
        .map(|(i, repo)| {
            let style = if i == wizard.selected_repo_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if i == wizard.selected_repo_index {
                "▸ "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(repo, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                " Repositories ",
                Style::default().fg(Color::DarkGray),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    f.render_widget(list, area);
}

fn draw_wizard_branch(f: &mut Frame, app: &mut App, area: Rect) {
    let wizard = match &mut app.wizard {
        Some(w) => w,
        None => return,
    };

    // Split into mode tabs and content
    // Use Length(3) for CreateNew (single-line input), Min(3) for SelectExisting (list)
    let content_constraint = match wizard.branch_mode {
        BranchMode::CreateNew => Constraint::Length(3),
        BranchMode::SelectExisting => Constraint::Min(3),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), content_constraint])
        .split(area);

    // Draw mode tabs
    let tab_titles = vec![
        Span::styled(
            " Create New ",
            if wizard.branch_mode == BranchMode::CreateNew {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            " Select Existing ",
            if wizard.branch_mode == BranchMode::SelectExisting {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ];

    let tabs = Tabs::new(tab_titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " Tab to switch mode ",
                    Style::default().fg(Color::DarkGray),
                )),
        )
        .select(match wizard.branch_mode {
            BranchMode::CreateNew => 0,
            BranchMode::SelectExisting => 1,
        })
        .highlight_style(Style::default().fg(Color::LightCyan));

    f.render_widget(tabs, chunks[0]);

    // Draw content based on mode
    match wizard.branch_mode {
        BranchMode::CreateNew => {
            wizard.new_branch_editor.set_block(
                Block::default()
                    .title(Span::styled(
                        " Enter branch name ",
                        Style::default().fg(Color::LightGreen),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightGreen)),
            );
            wizard
                .new_branch_editor
                .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
            f.render_widget(&wizard.new_branch_editor, chunks[1]);
        }
        BranchMode::SelectExisting => {
            if wizard.existing_branches.is_empty() {
                let msg = Paragraph::new("No available branches (all have tasks or repo is empty)")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(
                        Block::default()
                            .title(Span::styled(
                                " Existing Branches ",
                                Style::default().fg(Color::DarkGray),
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray)),
                    );
                f.render_widget(msg, chunks[1]);
            } else {
                let items: Vec<ListItem> = wizard
                    .existing_branches
                    .iter()
                    .enumerate()
                    .map(|(i, branch)| {
                        let style = if i == wizard.selected_branch_index {
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Rgb(40, 40, 60))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        let prefix = if i == wizard.selected_branch_index {
                            "▸ "
                        } else {
                            "  "
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(branch, style),
                        ]))
                    })
                    .collect();

                let list = List::new(items).block(
                    Block::default()
                        .title(Span::styled(
                            " Existing Branches ",
                            Style::default().fg(Color::LightYellow),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::LightYellow)),
                );

                f.render_widget(list, chunks[1]);
            }
        }
    }
}

fn draw_wizard_description(f: &mut Frame, app: &mut App, area: Rect) {
    let wizard = match &mut app.wizard {
        Some(w) => w,
        None => return,
    };

    let mode = wizard.description_editor.mode();
    let mode_color = match mode {
        VimMode::Normal => Color::LightCyan,
        VimMode::Insert => Color::LightGreen,
        VimMode::Visual => Color::LightYellow,
        VimMode::Operator(_) => Color::LightMagenta,
    };

    let title = format!(
        " Describe what this task should accomplish [{}] (Ctrl+S to continue) ",
        mode.indicator()
    );

    wizard.description_editor.textarea.set_block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default().fg(mode_color),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(mode_color)),
    );
    wizard
        .description_editor.textarea
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&wizard.description_editor.textarea, area);
}

fn draw_wizard_flow_list(f: &mut Frame, wizard: &super::app::NewTaskWizard, area: Rect) {
    let items: Vec<ListItem> = wizard
        .flows
        .iter()
        .enumerate()
        .map(|(i, flow)| {
            let style = if i == wizard.selected_flow_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if i == wizard.selected_flow_index {
                "▸ "
            } else {
                "  "
            };
            let default_marker = if flow == "default" { " (default)" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(flow, style),
                Span::styled(default_marker, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                " Select Flow (Enter to create task) ",
                Style::default().fg(Color::LightMagenta),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightMagenta)),
    );

    f.render_widget(list, area);
}

fn draw_wizard_footer_direct(
    f: &mut Frame,
    step: WizardStep,
    error_message: Option<String>,
    area: Rect,
) {
    let content = if let Some(err) = &error_message {
        Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::LightRed)),
            Span::styled(err, Style::default().fg(Color::LightRed)),
        ])
    } else {
        // Show contextual help
        let help = match step {
            WizardStep::SelectRepo => "j/k: navigate  Enter: select  Esc: cancel",
            WizardStep::SelectBranch => "Tab: switch mode  j/k: navigate  Enter: next  Esc: back",
            WizardStep::EnterDescription => "Ctrl+S: continue  Esc: back",
            WizardStep::SelectFlow => "j/k: navigate  Enter: create task  Esc: back",
        };
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    };

    let para = Paragraph::new(content);
    f.render_widget(para, area);
}

fn draw_command_list(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    // Split into header and list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Run command on: ", Style::default().fg(Color::DarkGray)),
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
                " Stored Commands ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightMagenta)),
    );
    f.render_widget(header, chunks[0]);

    // Command list
    let items: Vec<ListItem> = app
        .commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if i == app.selected_command_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if i == app.selected_command_index {
                "▸ "
            } else {
                "  "
            };

            let lines = vec![
                Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(&cmd.name, style),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(&cmd.description, Style::default().fg(Color::DarkGray)),
                ]),
            ];

            ListItem::new(lines)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                " Select a command (Enter to run, Esc to cancel) ",
                Style::default().fg(Color::LightGreen),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightGreen)),
    );

    f.render_widget(list, chunks[1]);
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
