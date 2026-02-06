use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::task::TaskStatus;

use super::app::{App, BranchSource, PreviewPane, ReviewWizardStep, View, WizardStep};
use super::vim::VimMode;

pub fn draw(f: &mut Frame, app: &mut App) {
    // Check if we're showing a modal that should hide the output pane
    let is_modal_view = matches!(
        app.view,
        View::DeleteConfirm
            | View::Feedback
            | View::NewTaskWizard
            | View::CommandList
            | View::TaskEditor
            | View::FeedbackQueue
            | View::RebaseBranchPicker
            | View::ReviewWizard
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
            draw_preview(f, app, chunks[0]);
            draw_command_list(f, app);
        }
        View::TaskEditor => {
            draw_preview(f, app, chunks[0]);
            draw_task_editor(f, app);
        }
        View::FeedbackQueue => {
            draw_preview(f, app, chunks[0]);
            draw_feedback_queue(f, app);
        }
        View::RebaseBranchPicker => {
            draw_preview(f, app, chunks[0]);
            draw_rebase_branch_picker(f, app);
        }
        View::ReviewWizard => {
            draw_task_list(f, app, chunks[0]);
            draw_review_wizard(f, app);
        }
    }

    if output_height > 0 {
        draw_output_pane(f, app, chunks[1]);
    }

    draw_status_bar(f, app, chunks[2]);
}

fn draw_task_list(f: &mut Frame, app: &App, area: Rect) {
    // Count running and stopped tasks
    let running_count = app
        .tasks
        .iter()
        .filter(|t| t.meta.status == TaskStatus::Running)
        .count();
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

    // Calculate dynamic column widths
    const MIN_REPO_WIDTH: usize = 4; // "REPO" header length
    const MAX_REPO_WIDTH: usize = 20;
    const MIN_BRANCH_WIDTH: usize = 6; // "BRANCH" header length

    const STATUS_WIDTH: usize = 10;
    const MIN_AGENT_WIDTH: usize = 6; // width of "AGENT" header + 1
    const MAX_AGENT_WIDTH: usize = 25;
    const UPDATED_WIDTH: usize = 10;
    const COL_GAP: &str = "   "; // 3 spaces between columns

    // Scan tasks for longest repo name
    let max_repo_len = app
        .tasks
        .iter()
        .map(|t| t.meta.repo_name.len())
        .max()
        .unwrap_or(MIN_REPO_WIDTH);

    let repo_width = max_repo_len
        .max(MIN_REPO_WIDTH)
        .min(MAX_REPO_WIDTH);

    // Scan tasks for longest branch name (including queue suffix)
    let max_branch_len = app
        .tasks
        .iter()
        .map(|t| {
            let queue_count = t.queued_feedback_count();
            let suffix_len = if queue_count > 0 {
                format!(" (+{})", queue_count).len()
            } else {
                0
            };
            t.meta.branch_name.len() + suffix_len
        })
        .max()
        .unwrap_or(MIN_BRANCH_WIDTH);

    let branch_width = max_branch_len
        .max(MIN_BRANCH_WIDTH);

    // Scan tasks for longest agent name
    let max_agent_len = app
        .tasks
        .iter()
        .filter_map(|t| {
            if t.meta.status == TaskStatus::Running {
                t.meta.current_agent.as_deref()
            } else {
                None
            }
        })
        .map(|a| a.len())
        .max()
        .unwrap_or(0);

    let agent_width = max_agent_len
        .max(MIN_AGENT_WIDTH)
        .min(MAX_AGENT_WIDTH);

    // Compute fixed width from actual components:
    // icon(1) + padding(3) + col_gaps(4*3=12) + status + agent + updated
    let fixed_cols_width = (1 + 3 + 12 + repo_width + STATUS_WIDTH + agent_width + UPDATED_WIDTH) as u16;

    let available_width = inner.width.saturating_sub(fixed_cols_width) as usize;

    // Cap branch width to available space
    let branch_width = branch_width.min(available_width.max(MIN_BRANCH_WIDTH));

    // Render header - columns: icon(1) + space(2) + repo + gap + branch + gap + status + gap + agent + gap + updated
    let header = Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<width$}", "REPO", width = repo_width),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(COL_GAP),
        Span::styled(
            format!("{:<width$}", "BRANCH", width = branch_width),
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
            format!("{:<width$}", "AGENT", width = agent_width),
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
            let label = format!("── Running ({}) ", running_count);
            let fill = (inner.width as usize).saturating_sub(label.len());
            let header_line = Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("─".repeat(fill), Style::default().fg(Color::Rgb(60, 60, 60))),
            ]);
            items.push(ListItem::new(header_line));
            shown_running_header = true;
        } else if !is_running && !shown_stopped_header && stopped_count > 0 {
            // Add spacing before stopped section if there were running tasks
            if shown_running_header {
                items.push(ListItem::new(Line::from("")));
            }
            let label = format!("── Stopped ({}) ", stopped_count);
            let fill = (inner.width as usize).saturating_sub(label.len());
            let header_line = Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Rgb(140, 140, 140))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("─".repeat(fill), Style::default().fg(Color::Rgb(60, 60, 60))),
            ]);
            items.push(ListItem::new(header_line));
            shown_stopped_header = true;
        }

        // Render the task
        let (status_icon, status_color) = match task.meta.status {
            TaskStatus::Running => ("●", Color::LightGreen),
            TaskStatus::Stopped => ("○", Color::Rgb(140, 140, 140)),
        };

        // Only show agent if task is running
        let agent_str = if is_running {
            task.meta.current_agent.as_deref().unwrap_or("-")
        } else {
            "-"
        };
        let status_str = format!("{}", task.meta.status);

        // Build display repo name (truncate if needed)
        let display_repo = if task.meta.repo_name.len() > repo_width {
            format!("{}…", &task.meta.repo_name[..repo_width.saturating_sub(1)])
        } else {
            task.meta.repo_name.clone()
        };

        // Build display branch name with optional queue indicator
        let queue_count = task.queued_feedback_count();
        let queue_suffix = if queue_count > 0 {
            format!(" (+{})", queue_count)
        } else {
            String::new()
        };
        let full_branch = format!("{}{}", task.meta.branch_name, queue_suffix);

        // Truncate branch if needed, with ellipsis
        let display_branch = if full_branch.len() > branch_width {
            format!("{}…", &full_branch[..branch_width.saturating_sub(1)])
        } else {
            full_branch.clone()
        };

        // Dim stopped tasks
        let text_color = if is_running {
            if task_index == app.selected_index {
                Color::White
            } else {
                Color::Gray
            }
        } else {
            Color::Rgb(140, 140, 140)
        };

        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(status_icon, Style::default().fg(status_color)),
            Span::raw("  "),
            Span::styled(
                format!("{:<width$}", display_repo, width = repo_width),
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
                format!("{:<width$}", display_branch, width = branch_width),
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
                format!("{:<width$}", agent_str, width = agent_width),
                if is_running {
                    Style::default().fg(Color::LightBlue)
                } else {
                    Style::default().fg(Color::Rgb(110, 110, 110))
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
        let queue_count = task.queued_feedback_count();

        let mut header_spans = vec![
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
        ];

        // Add queued feedback indicator if there are items in the queue
        if queue_count > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled("Queue: ", Style::default().fg(Color::DarkGray)));
            header_spans.push(Span::styled(
                format!("{}", queue_count),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let header = Paragraph::new(Line::from(header_spans))
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
        Style::default().fg(mode_color).add_modifier(Modifier::BOLD)
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
        app.notes_editor
            .textarea
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
    app.task_file_editor
        .textarea
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
    app.feedback_editor
        .textarea
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&app.feedback_editor.textarea, chunks[1]);
}

fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(55, 45, f.area());

    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let sel = app.delete_mode_index;

    let everything_style = if sel == 0 {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(60, 30, 30))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let task_only_style = if sel == 1 {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(60, 60, 20))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let everything_prefix = if sel == 0 { "▸ " } else { "  " };
    let task_only_prefix = if sel == 1 { "▸ " } else { "  " };

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  Delete task '{}'?", task_id),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{}Delete everything", everything_prefix),
            everything_style,
        )),
        Line::from(Span::styled(
            "    Kill tmux, remove worktree, delete branch,",
            Style::default().fg(Color::LightRed),
        )),
        Line::from(Span::styled(
            "    delete task files",
            Style::default().fg(Color::LightRed),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{}Delete task only", task_only_prefix),
            task_only_style,
        )),
        Line::from(Span::styled(
            "    Kill tmux, delete task files, remove TASK.md",
            Style::default().fg(Color::LightYellow),
        )),
        Line::from(Span::styled(
            "    Keep worktree and branch intact",
            Style::default().fg(Color::LightYellow),
        )),
    ];

    let popup = Paragraph::new(text).block(
        Block::default()
            .title(Span::styled(
                " Delete Task ",
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
    let lines: Vec<Line> = app
        .output_log
        .iter()
        .map(|line| {
            let lower = line.to_lowercase();
            let is_error = lower.contains("error")
                || lower.contains("failed")
                || lower.contains("[stderr]");
            let color = if is_error {
                Color::LightRed
            } else {
                Color::Gray
            };
            Line::from(Span::styled(line.as_str(), Style::default().fg(color)))
        })
        .collect();

    let output = Paragraph::new(lines)
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
                Span::styled("r", Style::default().fg(Color::LightGreen)),
                Span::styled(" review  ", Style::default().fg(Color::DarkGray)),
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
                    Span::styled("Q", Style::default().fg(Color::LightYellow)),
                    Span::styled(" queue  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("x", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" cmd  ", Style::default().fg(Color::DarkGray)),
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
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc/q", Style::default().fg(Color::LightRed)),
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
        View::FeedbackQueue => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("d", Style::default().fg(Color::LightRed)),
                Span::styled(" delete  ", Style::default().fg(Color::DarkGray)),
                Span::styled("C", Style::default().fg(Color::LightRed)),
                Span::styled(" clear all  ", Style::default().fg(Color::DarkGray)),
                Span::styled("q/Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" close", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::RebaseBranchPicker => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::ReviewWizard => {
            if let Some(wizard) = &app.review_wizard {
                match wizard.step {
                    ReviewWizardStep::SelectRepo => {
                        vec![
                            Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                            Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                    ReviewWizardStep::EnterBranch => {
                        vec![
                            Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                            Span::styled(" start review  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" back", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                }
            } else {
                vec![]
            }
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
    let (step, step_num, total_steps, step_title, error_message) = {
        let wizard = match &app.wizard {
            Some(w) => w,
            None => return,
        };
        let total = 3;
        let (step_num, step_title) = match wizard.step {
            WizardStep::SelectRepo => (1, "Select Repository"),
            WizardStep::SelectBranch => (2, "Branch / Worktree"),
            WizardStep::EnterDescription => (3, "Task Description"),
        };
        (
            wizard.step,
            step_num,
            total,
            step_title,
            wizard.error_message.clone(),
        )
    };

    // Main wizard container
    let block = Block::default()
        .title(Span::styled(
            format!(" New Task [{}/{}] {} ", step_num, total_steps, step_title),
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
    }

    // Draw error message or help text
    draw_wizard_footer_direct(f, step, error_message, chunks[1]);
}

fn draw_wizard_repo_list(f: &mut Frame, wizard: &super::app::NewTaskWizard, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut flat_index: usize = 0;

    // Favorites section
    if !wizard.favorite_repos.is_empty() {
        let header_line = Line::from(vec![
            Span::styled(
                format!("── Favorites ({}) ", wizard.favorite_repos.len()),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("─".repeat(40), Style::default().fg(Color::Rgb(60, 60, 60))),
        ]);
        items.push(ListItem::new(header_line));

        for (repo, count) in &wizard.favorite_repos {
            let is_selected = flat_index == wizard.selected_repo_index;
            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if is_selected { "▸ " } else { "  " };
            let count_str = format!("  ({} tasks)", count);
            items.push(ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(repo, style),
                Span::styled(count_str, Style::default().fg(Color::DarkGray)),
            ])));
            flat_index += 1;
        }

        // Spacing before All Repositories section
        items.push(ListItem::new(Line::from("")));
    }

    // All Repositories section header
    let header_line = Line::from(vec![
        Span::styled(
            format!("── All Repositories ({}) ", wizard.repos.len()),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(34), Style::default().fg(Color::Rgb(40, 40, 40))),
    ]);
    items.push(ListItem::new(header_line));

    for repo in &wizard.repos {
        let is_selected = flat_index == wizard.selected_repo_index;
        let style = if is_selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(40, 40, 60))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let prefix = if is_selected { "▸ " } else { "  " };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(repo, style),
        ])));
        flat_index += 1;
    }

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

    // Content area sizing: text input gets Length(3), lists get Min(3)
    let content_constraint = match wizard.branch_source {
        BranchSource::NewBranch => Constraint::Length(3),
        BranchSource::ExistingBranch | BranchSource::ExistingWorktree => Constraint::Min(3),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), content_constraint])
        .split(area);

    // Draw 3 tabs
    let tab_titles = vec![
        Span::styled(
            " New Branch ",
            if wizard.branch_source == BranchSource::NewBranch {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            " Existing Branch ",
            if wizard.branch_source == BranchSource::ExistingBranch {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else if !wizard.existing_branches.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Rgb(60, 60, 60))
            },
        ),
        Span::styled(
            " Existing Worktree ",
            if wizard.branch_source == BranchSource::ExistingWorktree {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else if !wizard.existing_worktrees.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Rgb(60, 60, 60))
            },
        ),
    ];

    let tabs = Tabs::new(tab_titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " Tab to switch ",
                    Style::default().fg(Color::DarkGray),
                )),
        )
        .select(match wizard.branch_source {
            BranchSource::NewBranch => 0,
            BranchSource::ExistingBranch => 1,
            BranchSource::ExistingWorktree => 2,
        })
        .highlight_style(Style::default().fg(Color::LightCyan));

    f.render_widget(tabs, chunks[0]);

    // Draw content for the selected tab
    match wizard.branch_source {
        BranchSource::NewBranch => {
            wizard.new_branch_editor.set_block(
                Block::default()
                    .title(Span::styled(
                        " Enter branch name (creates new branch + worktree) ",
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
        BranchSource::ExistingBranch => {
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
                            " Select branch (creates new worktree) ",
                            Style::default().fg(Color::LightYellow),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::LightYellow)),
                );

                f.render_widget(list, chunks[1]);
            }
        }
        BranchSource::ExistingWorktree => {
            if wizard.existing_worktrees.is_empty() {
                let msg =
                    Paragraph::new("No existing worktrees without tasks for this repository.")
                        .style(Style::default().fg(Color::DarkGray))
                        .block(
                            Block::default()
                                .title(Span::styled(
                                    " Existing Worktrees ",
                                    Style::default().fg(Color::DarkGray),
                                ))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::DarkGray)),
                        );
                f.render_widget(msg, chunks[1]);
            } else {
                let items: Vec<ListItem> = wizard
                    .existing_worktrees
                    .iter()
                    .enumerate()
                    .map(|(i, (branch, path))| {
                        let style = if i == wizard.selected_worktree_index {
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Rgb(40, 40, 60))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        let prefix = if i == wizard.selected_worktree_index {
                            "▸ "
                        } else {
                            "  "
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(branch, style),
                            Span::styled(
                                format!("  ({})", path.display()),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]))
                    })
                    .collect();

                let list = List::new(items).block(
                    Block::default()
                        .title(Span::styled(
                            " Select worktree (uses existing worktree) ",
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
            .title(Span::styled(title, Style::default().fg(mode_color)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(mode_color)),
    );
    wizard
        .description_editor
        .textarea
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&wizard.description_editor.textarea, area);
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
            WizardStep::EnterDescription => "Ctrl+S: create task  Esc: back",
        };
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    };

    let para = Paragraph::new(content);
    f.render_widget(para, area);
}

fn draw_command_list(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 70, f.area());
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

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Select a command (Enter to run, Esc to cancel) ",
                    Style::default().fg(Color::LightGreen),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightGreen)),
        )
        .highlight_style(Style::default());

    f.render_stateful_widget(list, chunks[1], &mut app.command_list_state);
}

fn draw_feedback_queue(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let queue = app
        .selected_task()
        .map(|t| t.read_feedback_queue().to_vec())
        .unwrap_or_default();

    // Split into header, list, and footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Queued feedback for: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            &task_id,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ({} items)", queue.len()),
            Style::default().fg(Color::LightYellow),
        ),
    ]))
    .block(
        Block::default()
            .title(Span::styled(
                " Feedback Queue ",
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightYellow)),
    );
    f.render_widget(header, chunks[0]);

    // Queue list
    let items: Vec<ListItem> = queue
        .iter()
        .enumerate()
        .map(|(i, feedback)| {
            let is_selected = i == app.selected_queue_index;
            let style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if is_selected { "▸ " } else { "  " };

            // Truncate feedback preview to fit on one line
            let preview = if feedback.len() > 60 {
                format!("{}...", &feedback[..57])
            } else {
                feedback.clone()
            };
            // Replace newlines with spaces for display
            let preview = preview.replace('\n', " ");

            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
                Span::styled(preview, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                " j/k: navigate  d: delete item  C: clear all  q: close ",
                Style::default().fg(Color::DarkGray),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(list, chunks[1]);

    // Selected item preview (if any)
    if let Some(feedback) = queue.get(app.selected_queue_index) {
        let preview_text = if feedback.len() > 200 {
            format!("{}...", &feedback[..197])
        } else {
            feedback.clone()
        };
        let preview = Paragraph::new(preview_text)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true });
        f.render_widget(preview, chunks[2]);
    }
}

fn draw_rebase_branch_picker(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    // Dynamic title and labels based on the pending command
    let (picker_title, header_label, list_title) = match app
        .pending_branch_command
        .as_ref()
        .map(|c| c.id.as_str())
    {
        Some("local-merge") => (
            " Merge Branch Picker ",
            "Merge task into: ",
            " Select branch to merge into (Enter to select, Esc to cancel) ",
        ),
        Some("rebase") => (
            " Rebase Branch Picker ",
            "Rebase task: ",
            " Select branch to rebase onto (Enter to select, Esc to cancel) ",
        ),
        _ => (
            " Branch Picker ",
            "Task: ",
            " Select branch (Enter to select, Esc to cancel) ",
        ),
    };

    // Split into header and list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled(header_label, Style::default().fg(Color::DarkGray)),
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
                picker_title,
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan)),
    );
    f.render_widget(header, chunks[0]);

    // Branch list
    let items: Vec<ListItem> = app
        .rebase_branches
        .iter()
        .enumerate()
        .map(|(i, branch)| {
            let style = if i == app.selected_rebase_branch_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if i == app.selected_rebase_branch_index {
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
                list_title,
                Style::default().fg(Color::LightGreen),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightGreen)),
    );

    f.render_widget(list, chunks[1]);
}

fn draw_review_wizard(f: &mut Frame, app: &mut App) {
    let area = centered_rect(70, 50, f.area());
    f.render_widget(Clear, area);

    let (step, step_num, step_title, error_message) = {
        let wizard = match &app.review_wizard {
            Some(w) => w,
            None => return,
        };
        let (step_num, step_title) = match wizard.step {
            ReviewWizardStep::SelectRepo => (1, "Select Repository"),
            ReviewWizardStep::EnterBranch => (2, "Enter Branch Name"),
        };
        (
            wizard.step,
            step_num,
            step_title,
            wizard.error_message.clone(),
        )
    };

    let block = Block::default()
        .title(Span::styled(
            format!(" Review Branch [{}/2] {} ", step_num, step_title),
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightMagenta));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(inner);

    match step {
        ReviewWizardStep::SelectRepo => {
            if let Some(wizard) = &app.review_wizard {
                // Reuse the same list rendering pattern as the new task wizard
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

                f.render_widget(list, chunks[0]);
            }
        }
        ReviewWizardStep::EnterBranch => {
            if let Some(wizard) = &mut app.review_wizard {
                wizard.branch_editor.set_block(
                    Block::default()
                        .title(Span::styled(
                            " Enter remote branch name to review ",
                            Style::default().fg(Color::LightGreen),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::LightGreen)),
                );
                wizard
                    .branch_editor
                    .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
                f.render_widget(&wizard.branch_editor, chunks[0]);
            }
        }
    }

    // Draw error or help text
    let content = if let Some(err) = &error_message {
        Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::LightRed)),
            Span::styled(err, Style::default().fg(Color::LightRed)),
        ])
    } else {
        let help = match step {
            ReviewWizardStep::SelectRepo => "j/k: navigate  Enter: select  Esc: cancel",
            ReviewWizardStep::EnterBranch => "Enter: start review  Esc: back",
        };
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    };

    f.render_widget(Paragraph::new(content), chunks[1]);
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
