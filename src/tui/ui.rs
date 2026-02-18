use chrono::{Local, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use agman::task::TaskStatus;

use std::time::Duration;

use super::app::{App, BranchSource, DirPickerOrigin, DirKind, NotesFocus, PreviewPane, RestartWizardStep, View, WizardStep, BREAK_INTERVAL, BREAK_WARNING_SECS};
use super::log_render;
use super::vim::VimMode;

fn clock_title(app: &App) -> Line<'static> {
    let elapsed = app.last_break_reset.elapsed();
    let break_spans = if elapsed >= BREAK_INTERVAL {
        vec![Span::styled(
            " \u{2615} BREAK ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(255, 140, 40))
                .add_modifier(Modifier::BOLD),
        )]
    } else if elapsed >= BREAK_INTERVAL - Duration::from_secs(BREAK_WARNING_SECS) {
        let remaining_mins = (BREAK_INTERVAL - elapsed).as_secs() / 60;
        vec![Span::styled(
            format!(" \u{2615} {}m ", remaining_mins),
            Style::default().fg(Color::Rgb(180, 140, 60)),
        )]
    } else {
        vec![]
    };

    let unread_count = app.notifications.iter().filter(|n| n.unread).count();

    let notif_spans = if !app.gh_notif_first_poll_done {
        // Loading state
        vec![Span::styled(
            " ✉ ... ",
            Style::default().fg(Color::DarkGray),
        )]
    } else if unread_count > 0 {
        // Unread notifications — prominent badge with amber background
        let amber = Color::Rgb(255, 180, 40);
        vec![Span::styled(
            format!(" \u{2709} {} ", unread_count),
            Style::default()
                .fg(Color::Black)
                .bg(amber)
                .add_modifier(Modifier::BOLD),
        )]
    } else {
        // Zero unread — hide notification indicator entirely
        vec![]
    };

    let clock_span = Span::styled(
        format!(" {} ", Local::now().format("%H:%M")),
        Style::default().fg(Color::DarkGray),
    );

    let mut spans = break_spans;
    spans.extend(notif_spans);
    spans.push(clock_span);

    Line::from(spans).alignment(Alignment::Right)
}

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
            | View::RestartConfirm
            | View::RestartWizard
            | View::SetLinkedPr
            | View::DirectoryPicker
            | View::SessionPicker
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
        View::RestartConfirm => {
            draw_task_list(f, app, chunks[0]);
            draw_restart_confirm(f, app);
        }
        View::RestartWizard => {
            draw_preview(f, app, chunks[0]);
            draw_restart_wizard(f, app);
        }
        View::SetLinkedPr => {
            draw_task_list(f, app, chunks[0]);
            draw_set_linked_pr(f, app);
        }
        View::DirectoryPicker => {
            draw_task_list(f, app, chunks[0]);
            draw_directory_picker(f, app);
        }
        View::SessionPicker => {
            draw_preview(f, app, chunks[0]);
            draw_session_picker(f, app);
        }
        View::Notifications => draw_notifications(f, app, chunks[0]),
        View::Notes => draw_notes(f, app, chunks[0]),
        View::ShowPrs => draw_show_prs(f, app, chunks[0]),
    }

    if output_height > 0 {
        draw_output_pane(f, app, chunks[1]);
    }

    draw_status_bar(f, app, chunks[2]);
}

fn draw_task_list(f: &mut Frame, app: &App, area: Rect) {
    // Count tasks by status
    let running_count = app
        .tasks
        .iter()
        .filter(|t| t.meta.status == TaskStatus::Running)
        .count();
    let input_needed_count = app
        .tasks
        .iter()
        .filter(|t| t.meta.status == TaskStatus::InputNeeded)
        .count();
    let on_hold_count = app
        .tasks
        .iter()
        .filter(|t| t.meta.status == TaskStatus::OnHold)
        .count();
    let stopped_count = app.tasks.len() - running_count - input_needed_count - on_hold_count;

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
        .title(clock_title(app))
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

    const PR_WIDTH: usize = 25; // fits "#99999 (long_author_name)" plus padding
    const STATUS_WIDTH: usize = 10;
    const MIN_AGENT_WIDTH: usize = 6; // width of "AGENT" header + 1
    const MAX_AGENT_WIDTH: usize = 25;
    const UPDATED_WIDTH: usize = 10;
    const COL_GAP: &str = "   "; // 3 spaces between columns

    // Scan tasks for longest repo name (multi-repo tasks get [M] prefix)
    let max_repo_len = app
        .tasks
        .iter()
        .map(|t| {
            if t.meta.is_multi_repo() {
                t.meta.name.len() + 4 // "[M] " prefix
            } else {
                t.meta.name.len()
            }
        })
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
            if t.meta.status == TaskStatus::Running || t.meta.status == TaskStatus::InputNeeded {
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
    // icon(1) + padding(3) + col_gaps(5*3=15) + repo + pr + status + agent + updated
    let fixed_cols_width = (1 + 3 + 15 + repo_width + PR_WIDTH + STATUS_WIDTH + agent_width + UPDATED_WIDTH) as u16;

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
            format!("{:<width$}", "PR", width = PR_WIDTH),
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

    // Build task list (sorted by status: running, input_needed, stopped; then by updated_at)
    let mut items: Vec<ListItem> = Vec::new();
    let mut task_index = 0;
    let mut shown_running_header = false;
    let mut shown_input_needed_header = false;
    let mut shown_stopped_header = false;
    let mut shown_on_hold_header = false;

    for task in &app.tasks {
        let status = task.meta.status;

        // Add section header if needed
        match status {
            TaskStatus::Running if !shown_running_header && running_count > 0 => {
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
            }
            TaskStatus::InputNeeded if !shown_input_needed_header && input_needed_count > 0 => {
                if shown_running_header {
                    items.push(ListItem::new(Line::from("")));
                }
                let label = format!("── Input Needed ({}) ", input_needed_count);
                let fill = (inner.width as usize).saturating_sub(label.len());
                let header_line = Line::from(vec![
                    Span::styled(
                        label,
                        Style::default()
                            .fg(Color::LightYellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("─".repeat(fill), Style::default().fg(Color::Rgb(60, 60, 60))),
                ]);
                items.push(ListItem::new(header_line));
                shown_input_needed_header = true;
            }
            TaskStatus::Stopped if !shown_stopped_header && stopped_count > 0 => {
                if shown_running_header || shown_input_needed_header {
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
            TaskStatus::OnHold if !shown_on_hold_header && on_hold_count > 0 => {
                if shown_running_header || shown_input_needed_header || shown_stopped_header {
                    items.push(ListItem::new(Line::from("")));
                }
                let label = format!("── On Hold ({}) ", on_hold_count);
                let fill = (inner.width as usize).saturating_sub(label.len());
                let header_line = Line::from(vec![
                    Span::styled(
                        label,
                        Style::default()
                            .fg(Color::Rgb(180, 140, 60))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("─".repeat(fill), Style::default().fg(Color::Rgb(60, 60, 60))),
                ]);
                items.push(ListItem::new(header_line));
                shown_on_hold_header = true;
            }
            _ => {}
        }

        // Render the task
        let (status_icon, status_color) = match task.meta.status {
            TaskStatus::Running => ("●", Color::LightGreen),
            TaskStatus::InputNeeded => ("?", Color::LightYellow),
            TaskStatus::Stopped if !task.meta.seen => ("●", Color::Rgb(100, 200, 220)),
            TaskStatus::Stopped => ("○", Color::Rgb(140, 140, 140)),
            TaskStatus::OnHold => ("⏸", Color::Rgb(180, 140, 60)),
        };

        // Show agent for running and input_needed tasks
        let is_active = status == TaskStatus::Running || status == TaskStatus::InputNeeded;
        let agent_str = if is_active {
            task.meta.current_agent.as_deref().unwrap_or("-")
        } else {
            "-"
        };
        let status_str = format!("{}", task.meta.status);

        // Build display repo name (truncate if needed, prefix multi-repo tasks)
        let repo_label = if task.meta.is_multi_repo() {
            format!("[M] {}", task.meta.name)
        } else {
            task.meta.name.clone()
        };
        let display_repo = if repo_label.len() > repo_width {
            format!("{}…", &repo_label[..repo_width.saturating_sub(1)])
        } else {
            repo_label
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

        // Dim stopped tasks, brighten unread stopped, highlight active
        let text_color = if is_active {
            if task_index == app.selected_index {
                Color::White
            } else {
                Color::Gray
            }
        } else if task.meta.status == TaskStatus::Stopped && !task.meta.seen {
            Color::Rgb(200, 200, 200)
        } else {
            Color::Rgb(140, 140, 140)
        };

        // Build PR display string and truncate if needed
        let pr_display = task.meta.linked_pr.as_ref().map(|pr| {
            if !pr.owned {
                format!("#{:>5} ({})", pr.number, pr.author.as_deref().unwrap_or("ext"))
            } else if task.meta.review_addressed {
                format!("#{:>5} ✓", pr.number)
            } else {
                format!("#{:>5} mine", pr.number)
            }
        }).unwrap_or_default();
        let pr_display = if pr_display.len() > PR_WIDTH {
            format!("{}…", &pr_display[..PR_WIDTH.saturating_sub(1)])
        } else {
            pr_display
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
                format!("{:<width$}", pr_display, width = PR_WIDTH),
                if let Some(pr) = &task.meta.linked_pr {
                    if !pr.owned {
                        Style::default().fg(Color::Gray)
                    } else if task.meta.review_addressed {
                        Style::default().fg(Color::LightGreen)
                    } else {
                        Style::default().fg(Color::LightMagenta)
                    }
                } else {
                    Style::default()
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
                if is_active {
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
        let is_active = task.meta.status == TaskStatus::Running || task.meta.status == TaskStatus::InputNeeded;
        let agent_str = if is_active {
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
                    TaskStatus::InputNeeded => Color::LightYellow,
                    TaskStatus::Stopped => Color::DarkGray,
                    TaskStatus::OnHold => Color::Rgb(180, 140, 60),
                }),
            ),
            Span::raw("  "),
            Span::styled("Agent: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                agent_str,
                if is_active {
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
                .title(clock_title(app))
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

fn clamp_log_scroll(paragraph: &Paragraph, area: Rect, scroll: u16) -> u16 {
    let inner = area.inner(Margin::new(1, 1));
    let total_rows = paragraph.line_count(inner.width) as u16;
    let max_scroll = total_rows.saturating_sub(inner.height);
    scroll.min(max_scroll)
}

fn draw_logs_panel(f: &mut Frame, app: &mut App, area: Rect) {
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

    let styled_lines = log_render::render_log_lines(&app.preview_content);

    let logs = Paragraph::new(styled_lines)
        .block(
            Block::default()
                .title(Span::styled(" Logs (Enter: attach tmux) ", title_style))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false });

    app.preview_scroll = clamp_log_scroll(&logs, area, app.preview_scroll);

    let logs = logs.scroll((app.preview_scroll, 0));

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

    let is_answering = app.selected_task().map_or(false, |t| t.meta.status == TaskStatus::InputNeeded);
    let title_text = if is_answering {
        " Answer Questions "
    } else {
        " TASK.md Editor "
    };
    let title_color = if is_answering {
        Color::LightYellow
    } else {
        Color::LightMagenta
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header
    let mut header_spans = vec![
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
    ];
    if is_answering {
        header_spans.push(Span::styled(
            "  Fill in your answers under [ANSWERS], then save to resume",
            Style::default().fg(Color::LightYellow),
        ));
    }
    let header = Paragraph::new(Line::from(header_spans))
    .block(
        Block::default()
            .title(Span::styled(
                title_text,
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(title_color)),
    );
    f.render_widget(header, chunks[0]);

    // Editor
    let save_hint = if is_answering {
        " Ctrl+S to save & resume flow, Esc (in normal) to cancel "
    } else {
        " Ctrl+S to save & close, Esc (in normal) to cancel "
    };
    app.task_file_editor.textarea.set_block(
        Block::default()
            .title(Span::styled(
                save_hint,
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

    let (task_id, review_after) = app
        .selected_task()
        .map(|t| (t.meta.task_id(), t.meta.review_after))
        .unwrap_or_else(|| ("unknown".to_string(), false));

    let mode = app.feedback_editor.mode();
    let mode_color = match mode {
        VimMode::Normal => Color::LightCyan,
        VimMode::Insert => Color::LightGreen,
        VimMode::Visual => Color::LightYellow,
        VimMode::Operator(_) => Color::LightMagenta,
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(1)])
        .split(area);

    // Header
    let mut header_spans = vec![
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
    ];
    if review_after {
        header_spans.push(Span::styled("  [review: ON]", Style::default().fg(Color::LightGreen)));
    }
    let header = Paragraph::new(Line::from(header_spans))
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

    // Review toggle line
    let review_label = if review_after {
        Line::from(vec![
            Span::styled("  End with review: ", Style::default().fg(Color::DarkGray)),
            Span::styled("YES", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::styled("  (Ctrl+R to toggle)", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  End with review: ", Style::default().fg(Color::DarkGray)),
            Span::styled("no", Style::default().fg(Color::DarkGray)),
            Span::styled("  (Ctrl+R to toggle)", Style::default().fg(Color::DarkGray)),
        ])
    };
    f.render_widget(Paragraph::new(review_label), chunks[2]);
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

fn draw_restart_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 35, f.area());

    f.render_widget(Clear, area);

    let sel = app.restart_confirm_index;

    let restart_style = if sel == 0 {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(30, 40, 60))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let later_style = if sel == 1 {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(30, 40, 60))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let restart_prefix = if sel == 0 { "▸ " } else { "  " };
    let later_prefix = if sel == 1 { "▸ " } else { "  " };

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  A new version of agman is available.",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{}Restart now", restart_prefix),
            restart_style,
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("{}Later", later_prefix),
            later_style,
        )),
    ];

    let popup = Paragraph::new(text).block(
        Block::default()
            .title(Span::styled(
                " Restart agman ",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan)),
    );

    f.render_widget(popup, area);
}

fn draw_restart_wizard(f: &mut Frame, app: &mut App) {
    let wizard = match &app.restart_wizard {
        Some(w) => w,
        None => return,
    };

    match wizard.step {
        RestartWizardStep::EditTask => {
            let area = centered_rect(80, 70, f.area());
            f.render_widget(Clear, area);

            let task_id = wizard.task_id.clone();
            let mode = wizard.task_editor.mode();
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
                Span::styled("Task: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    &task_id,
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
                Span::styled(
                    "  Ctrl+S save & next, Tab skip, Esc cancel",
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
            .block(
                Block::default()
                    .title(Span::styled(
                        " Rerun: Edit TASK.md ",
                        Style::default()
                            .fg(Color::LightYellow)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightYellow)),
            );
            f.render_widget(header, chunks[0]);

            // Editor
            let wizard = app.restart_wizard.as_mut().unwrap();
            wizard.task_editor.textarea.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightYellow)),
            );
            f.render_widget(&wizard.task_editor.textarea, chunks[1]);
        }
        RestartWizardStep::SelectAgent => {
            let area = centered_rect(60, 50, f.area());
            f.render_widget(Clear, area);

            let task_id = wizard.task_id.clone();

            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  Rerunning: {}", task_id),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  Select which flow step to rerun from:",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ];

            for (i, label) in wizard.flow_steps.iter().enumerate() {
                let is_selected = i == wizard.selected_step_index;
                let prefix = if is_selected { "▸ " } else { "  " };
                let style = if is_selected {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(30, 50, 30))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}{}", prefix, label),
                    style,
                )));
            }

            let popup = Paragraph::new(lines).block(
                Block::default()
                    .title(Span::styled(
                        " Rerun: Pick Starting Step ",
                        Style::default()
                            .fg(Color::LightYellow)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightYellow)),
            );

            f.render_widget(popup, area);
        }
    }
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

fn break_hint_spans(app: &App) -> Vec<Span<'static>> {
    let break_due = app.last_break_reset.elapsed() >= BREAK_INTERVAL;
    if break_due {
        vec![
            Span::styled(
                "B",
                Style::default()
                    .fg(Color::Rgb(255, 140, 40))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" break  ", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![
            Span::styled("B", Style::default().fg(Color::Rgb(180, 140, 60))),
            Span::styled(" break  ", Style::default().fg(Color::DarkGray)),
        ]
    }
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.view {
        View::TaskList => {
            let mut spans = vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("n", Style::default().fg(Color::LightGreen)),
                Span::styled(" new  ", Style::default().fg(Color::DarkGray)),
                Span::styled("r", Style::default().fg(Color::LightGreen)),
                Span::styled(" review  ", Style::default().fg(Color::DarkGray)),
            ];
            if let Some(task) = app.selected_task() {
                // State-conditional hints
                if task.meta.status == TaskStatus::InputNeeded {
                    spans.push(Span::styled("a", Style::default().fg(Color::LightYellow)));
                    spans.push(Span::styled(" answer  ", Style::default().fg(Color::DarkGray)));
                }
                if task.meta.linked_pr.is_some() {
                    spans.push(Span::styled("o", Style::default().fg(Color::LightYellow)));
                    spans.push(Span::styled(" PR  ", Style::default().fg(Color::DarkGray)));
                }
                if task.meta.status == TaskStatus::Stopped {
                    spans.push(Span::styled("H", Style::default().fg(Color::Rgb(180, 140, 60))));
                    spans.push(Span::styled(" hold  ", Style::default().fg(Color::DarkGray)));
                } else if task.meta.status == TaskStatus::OnHold {
                    spans.push(Span::styled("H", Style::default().fg(Color::Rgb(180, 140, 60))));
                    spans.push(Span::styled(" unhold  ", Style::default().fg(Color::DarkGray)));
                }
                if task.meta.review_addressed && task.meta.linked_pr.as_ref().is_some_and(|pr| pr.owned) {
                    spans.push(Span::styled("c", Style::default().fg(Color::LightGreen)));
                    spans.push(Span::styled(" clear ✓  ", Style::default().fg(Color::DarkGray)));
                }
                if task.meta.status == TaskStatus::Running {
                    spans.push(Span::styled("S", Style::default().fg(Color::LightRed)));
                    spans.push(Span::styled(" stop  ", Style::default().fg(Color::DarkGray)));
                }
                // Task-selected hints (always shown when a task is selected)
                spans.extend([
                    Span::styled("R", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" rerun  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("P", Style::default().fg(Color::LightYellow)),
                    Span::styled(" PR  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("t", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" task  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("x", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" cmd  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("f", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("d", Style::default().fg(Color::LightCyan)),
                    Span::styled(" del  ", Style::default().fg(Color::DarkGray)),
                ]);
            }
            let unread_count = app.notifications.iter().filter(|n| n.unread).count();
            let notif_label = if unread_count > 0 {
                format!(" notif({})  ", unread_count)
            } else if !app.gh_notif_first_poll_done {
                " notif(...)  ".to_string()
            } else {
                " notif  ".to_string()
            };
            spans.extend([
                Span::styled("N", Style::default().fg(Color::LightYellow)),
                Span::styled(notif_label, Style::default().fg(Color::DarkGray)),
                Span::styled("I", Style::default().fg(Color::LightYellow)),
                Span::styled(" prs  ", Style::default().fg(Color::DarkGray)),
                Span::styled("m", Style::default().fg(Color::LightYellow)),
                Span::styled(" notes  ", Style::default().fg(Color::DarkGray)),
            ]);
            spans.extend(break_hint_spans(app));
            spans.extend([
                Span::styled("q", Style::default().fg(Color::LightCyan)),
                Span::styled(" quit", Style::default().fg(Color::DarkGray)),
            ]);
            spans
        }
        View::Preview => {
            if app.notes_editing {
                vec![
                    Span::styled("Esc", Style::default().fg(Color::LightGreen)),
                    Span::styled(" save & exit editing", Style::default().fg(Color::DarkGray)),
                ]
            } else {
                let mut spans = vec![
                    Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                    Span::styled(" pane  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
                ];
                if let Some(task) = app.selected_task() {
                    // State-conditional hints
                    if task.meta.status == TaskStatus::InputNeeded {
                        spans.push(Span::styled("a", Style::default().fg(Color::LightYellow)));
                        spans.push(Span::styled(" answer  ", Style::default().fg(Color::DarkGray)));
                    }
                    if task.meta.linked_pr.is_some() {
                        spans.push(Span::styled("o", Style::default().fg(Color::LightYellow)));
                        spans.push(Span::styled(" PR  ", Style::default().fg(Color::DarkGray)));
                    }
                    if task.meta.status == TaskStatus::Stopped {
                        spans.push(Span::styled("H", Style::default().fg(Color::Rgb(180, 140, 60))));
                        spans.push(Span::styled(" hold  ", Style::default().fg(Color::DarkGray)));
                    } else if task.meta.status == TaskStatus::OnHold {
                        spans.push(Span::styled("H", Style::default().fg(Color::Rgb(180, 140, 60))));
                        spans.push(Span::styled(" unhold  ", Style::default().fg(Color::DarkGray)));
                    }
                    if task.meta.status == TaskStatus::Running {
                        spans.push(Span::styled("S", Style::default().fg(Color::LightRed)));
                        spans.push(Span::styled(" stop  ", Style::default().fg(Color::DarkGray)));
                    }
                    // Task-selected hints (always shown when a task is selected)
                    spans.extend([
                        Span::styled("R", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" rerun  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("P", Style::default().fg(Color::LightYellow)),
                        Span::styled(" PR  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("t", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" task  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("f", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" feedback  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("x", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" cmd  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans.extend([
                    Span::styled("Q", Style::default().fg(Color::LightYellow)),
                    Span::styled(" queue  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("i", Style::default().fg(Color::LightCyan)),
                    Span::styled(" edit  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::LightCyan)),
                    Span::styled(" attach  ", Style::default().fg(Color::DarkGray)),
                ]);
                spans.extend(break_hint_spans(app));
                spans.extend([
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]);
                spans
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
        View::RebaseBranchPicker | View::SessionPicker => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::RestartConfirm => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc/q", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::RestartWizard => {
            if let Some(wizard) = &app.restart_wizard {
                match wizard.step {
                    RestartWizardStep::EditTask => {
                        vec![
                            Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                            Span::styled(" save & next  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                            Span::styled(" skip  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                    RestartWizardStep::SelectAgent => {
                        vec![
                            Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                            Span::styled(" rerun  ", Style::default().fg(Color::DarkGray)),
                            Span::styled("Esc", Style::default().fg(Color::LightRed)),
                            Span::styled(" back", Style::default().fg(Color::DarkGray)),
                        ]
                    }
                }
            } else {
                vec![]
            }
        }
        View::ReviewWizard => {
            if app.review_wizard.is_some() {
                vec![
                    Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                    Span::styled(" mode  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                    Span::styled(" start review  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::LightRed)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]
            } else {
                vec![]
            }
        }
        View::SetLinkedPr => {
            vec![
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::DirectoryPicker => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("l/Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
                Span::styled("h", Style::default().fg(Color::LightGreen)),
                Span::styled(" up  ", Style::default().fg(Color::DarkGray)),
                Span::styled("s", Style::default().fg(Color::LightGreen)),
                Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightRed)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::Notifications => {
            let mut spans = vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("d", Style::default().fg(Color::LightRed)),
                Span::styled(" done  ", Style::default().fg(Color::DarkGray)),
                Span::styled("o", Style::default().fg(Color::LightGreen)),
                Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
            ];
            spans.extend(break_hint_spans(app));
            spans.extend([
                Span::styled("q", Style::default().fg(Color::LightCyan)),
                Span::styled(" back", Style::default().fg(Color::DarkGray)),
            ]);
            spans
        }
        View::ShowPrs => {
            let mut spans = vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("o", Style::default().fg(Color::LightGreen)),
                Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
                Span::styled("R", Style::default().fg(Color::LightYellow)),
                Span::styled(" refresh  ", Style::default().fg(Color::DarkGray)),
            ];
            spans.extend(break_hint_spans(app));
            spans.extend([
                Span::styled("q", Style::default().fg(Color::LightCyan)),
                Span::styled(" back", Style::default().fg(Color::DarkGray)),
            ]);
            spans
        }
        View::Notes => {
            let is_editor = app.notes_view.as_ref()
                .map(|nv| nv.focus == NotesFocus::Editor)
                .unwrap_or(false);
            if is_editor {
                vec![
                    Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                    Span::styled(" explorer  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Ctrl+s", Style::default().fg(Color::LightGreen)),
                    Span::styled(" save  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]
            } else {
                let mut spans = vec![
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("J/K", Style::default().fg(Color::LightCyan)),
                    Span::styled(" reorder  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("l", Style::default().fg(Color::LightGreen)),
                    Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("h", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("a", Style::default().fg(Color::LightGreen)),
                    Span::styled(" new  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("A", Style::default().fg(Color::LightGreen)),
                    Span::styled(" dir  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("d", Style::default().fg(Color::LightRed)),
                    Span::styled(" del  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("x", Style::default().fg(Color::LightYellow)),
                    Span::styled(" cut  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("p", Style::default().fg(Color::LightGreen)),
                    Span::styled(" paste  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("r", Style::default().fg(Color::LightYellow)),
                    Span::styled(" rename  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                    Span::styled(" editor  ", Style::default().fg(Color::DarkGray)),
                ];
                spans.extend(break_hint_spans(app));
                spans.extend([
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]);
                spans
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
    let (step, step_num, total_steps, step_title, repo_name, error_message) = {
        let wizard = match &app.wizard {
            Some(w) => w,
            None => return,
        };
        let total = 2;
        let (step_num, step_title) = match wizard.step {
            WizardStep::SelectBranch => (1, "Branch / Worktree"),
            WizardStep::EnterDescription => (2, "Task Description"),
        };
        (
            wizard.step,
            step_num,
            total,
            step_title,
            wizard.selected_repo.clone(),
            wizard.error_message.clone(),
        )
    };

    // Build title showing the repo and step info
    let multi_prefix = if app.wizard.as_ref().is_some_and(|w| w.is_multi_repo) {
        "[multi] "
    } else {
        ""
    };
    let title_text = format!(
        " New Task: {}{} [{}/{}] {} ",
        multi_prefix, repo_name, step_num, total_steps, step_title
    );

    // Main wizard container
    let block = Block::default()
        .title(Span::styled(
            title_text,
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
        WizardStep::SelectBranch => draw_wizard_branch(f, app, chunks[0]),
        WizardStep::EnterDescription => draw_wizard_description(f, app, chunks[0]),
    }

    // Draw error message or help text
    draw_wizard_footer_direct(f, step, error_message, chunks[1]);
}

fn draw_wizard_branch(f: &mut Frame, app: &mut App, area: Rect) {
    let wizard = match &mut app.wizard {
        Some(w) => w,
        None => return,
    };

    draw_branch_tabs(
        f,
        wizard.branch_source,
        &mut wizard.new_branch_editor,
        Some(&mut wizard.base_branch_editor),
        wizard.base_branch_focus,
        &wizard.existing_branches,
        wizard.selected_branch_index,
        &wizard.existing_worktrees,
        wizard.selected_worktree_index,
        " New Branch ",
        " Enter branch name (creates new branch + worktree) ",
        area,
    );
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

    let review_indicator = if wizard.review_after {
        " [review: ON]"
    } else {
        ""
    };

    let title = format!(
        " Describe task goal (empty = setup only) [{}]{} (Ctrl+S to continue) ",
        mode.indicator(),
        review_indicator,
    );

    // Split area: editor on top, review toggle indicator at bottom
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

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

    f.render_widget(&wizard.description_editor.textarea, chunks[0]);

    // Review toggle line
    let review_label = if wizard.review_after {
        Line::from(vec![
            Span::styled("  End with review: ", Style::default().fg(Color::DarkGray)),
            Span::styled("YES", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::styled("  (Ctrl+R to toggle)", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  End with review: ", Style::default().fg(Color::DarkGray)),
            Span::styled("no", Style::default().fg(Color::DarkGray)),
            Span::styled("  (Ctrl+R to toggle)", Style::default().fg(Color::DarkGray)),
        ])
    };
    f.render_widget(Paragraph::new(review_label), chunks[1]);
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
            WizardStep::SelectBranch => "Tab: switch mode  j/k: navigate  Enter: next  Esc: back",
            WizardStep::EnterDescription => "Ctrl+S: create task (empty = setup only)  Ctrl+R: toggle review  Esc: back",
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
        .map(|t| t.read_feedback_queue())
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

fn draw_session_picker(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 50, f.area());
    f.render_widget(Clear, area);

    let task_id = app
        .selected_task()
        .map(|t| t.meta.task_id())
        .unwrap_or_else(|| "unknown".to_string());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    let header = Paragraph::new(Line::from(vec![
        Span::styled("Task: ", Style::default().fg(Color::DarkGray)),
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
                " Attach to Session ",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan)),
    );
    f.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = app
        .session_picker_sessions
        .iter()
        .enumerate()
        .map(|(i, (repo_name, _session))| {
            let style = if i == app.selected_session_index {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if i == app.selected_session_index {
                "▸ "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(repo_name, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                " Select repo session (Enter to attach, Esc to cancel) ",
                Style::default().fg(Color::LightGreen),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightGreen)),
    );

    f.render_widget(list, chunks[1]);
}

fn draw_branch_tabs(
    f: &mut Frame,
    branch_source: BranchSource,
    branch_editor: &mut tui_textarea::TextArea<'static>,
    base_branch_editor: Option<&mut tui_textarea::TextArea<'static>>,
    base_branch_focus: bool,
    existing_branches: &[String],
    selected_branch_index: usize,
    existing_worktrees: &[(String, std::path::PathBuf)],
    selected_worktree_index: usize,
    first_tab_label: &str,
    new_branch_label: &str,
    area: Rect,
) {
    let has_base = base_branch_editor.is_some();
    let content_constraint = match branch_source {
        BranchSource::NewBranch => {
            if has_base { Constraint::Length(7) } else { Constraint::Length(3) }
        }
        BranchSource::ExistingBranch | BranchSource::ExistingWorktree => Constraint::Min(3),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), content_constraint])
        .split(area);

    // Draw 3 tabs
    let tab_titles = vec![
        Span::styled(
            first_tab_label,
            if branch_source == BranchSource::NewBranch {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            " Existing Branch ",
            if branch_source == BranchSource::ExistingBranch {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else if !existing_branches.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Rgb(60, 60, 60))
            },
        ),
        Span::styled(
            " Existing Worktree ",
            if branch_source == BranchSource::ExistingWorktree {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else if !existing_worktrees.is_empty() {
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
        .select(match branch_source {
            BranchSource::NewBranch => 0,
            BranchSource::ExistingBranch => 1,
            BranchSource::ExistingWorktree => 2,
        })
        .highlight_style(Style::default().fg(Color::LightCyan));

    f.render_widget(tabs, chunks[0]);

    // Draw content for the selected tab
    match branch_source {
        BranchSource::NewBranch => {
            if let Some(base_editor) = base_branch_editor {
                // Two stacked fields: branch name + base branch
                let field_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Length(1), Constraint::Length(3)])
                    .split(chunks[1]);

                let branch_focused = !base_branch_focus;
                let branch_border_color = if branch_focused { Color::LightGreen } else { Color::DarkGray };
                let base_border_color = if base_branch_focus { Color::LightGreen } else { Color::DarkGray };
                let branch_title_color = if branch_focused { Color::LightGreen } else { Color::DarkGray };
                let base_title_color = if base_branch_focus { Color::LightGreen } else { Color::DarkGray };

                branch_editor.set_block(
                    Block::default()
                        .title(Span::styled(
                            new_branch_label.to_string(),
                            Style::default().fg(branch_title_color),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(branch_border_color)),
                );
                if branch_focused {
                    branch_editor.set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
                } else {
                    branch_editor.set_cursor_style(Style::default());
                }
                f.render_widget(&*branch_editor, field_chunks[0]);

                base_editor.set_block(
                    Block::default()
                        .title(Span::styled(
                            " Base branch (↑↓ to switch) ",
                            Style::default().fg(base_title_color),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(base_border_color)),
                );
                if base_branch_focus {
                    base_editor.set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
                } else {
                    base_editor.set_cursor_style(Style::default());
                }
                f.render_widget(&*base_editor, field_chunks[2]);
            } else {
                // Single field (no base branch editor — used by review wizard)
                branch_editor.set_block(
                    Block::default()
                        .title(Span::styled(
                            new_branch_label.to_string(),
                            Style::default().fg(Color::LightGreen),
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::LightGreen)),
                );
                branch_editor.set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
                f.render_widget(&*branch_editor, chunks[1]);
            }
        }
        BranchSource::ExistingBranch => {
            if existing_branches.is_empty() {
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
                let items: Vec<ListItem> = existing_branches
                    .iter()
                    .enumerate()
                    .map(|(i, branch)| {
                        let style = if i == selected_branch_index {
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Rgb(40, 40, 60))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        let prefix = if i == selected_branch_index {
                            "▸ "
                        } else {
                            "  "
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(branch.as_str(), style),
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
            if existing_worktrees.is_empty() {
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
                let items: Vec<ListItem> = existing_worktrees
                    .iter()
                    .enumerate()
                    .map(|(i, (branch, path))| {
                        let style = if i == selected_worktree_index {
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Rgb(40, 40, 60))
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        let prefix = if i == selected_worktree_index {
                            "▸ "
                        } else {
                            "  "
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(branch.as_str(), style),
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

fn draw_review_wizard(f: &mut Frame, app: &mut App) {
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);

    let error_message = {
        let wizard = match &app.review_wizard {
            Some(w) => w,
            None => return,
        };
        wizard.error_message.clone()
    };

    let block = Block::default()
        .title(Span::styled(
            " Review Branch — Branch / Worktree ",
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

    if let Some(wizard) = &mut app.review_wizard {
        draw_branch_tabs(
            f,
            wizard.branch_source,
            &mut wizard.branch_editor,
            None,
            false,
            &wizard.existing_branches,
            wizard.selected_branch_index,
            &wizard.existing_worktrees,
            wizard.selected_worktree_index,
            " Enter Branch ",
            " Enter branch name (will look up upstream if not local) ",
            chunks[0],
        );
    }

    // Draw error or help text
    let content = if let Some(err) = &error_message {
        Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::LightRed)),
            Span::styled(err, Style::default().fg(Color::LightRed)),
        ])
    } else {
        Line::from(Span::styled(
            "Tab: switch source  Enter: start review  Esc: back",
            Style::default().fg(Color::DarkGray),
        ))
    };

    f.render_widget(Paragraph::new(content), chunks[1]);
}

fn draw_set_linked_pr(f: &mut Frame, app: &App) {
    let area = centered_rect(40, 20, f.area());
    f.render_widget(Clear, area);

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    let block = Block::default()
        .title(Span::styled(
            " Set Linked PR ",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightCyan));
    f.render_widget(block, area);

    let prompt = Paragraph::new(Span::styled(
        "Enter PR number (empty to clear):",
        Style::default().fg(Color::White),
    ));
    f.render_widget(prompt, chunks[0]);

    f.render_widget(&app.pr_number_editor, chunks[2]);

    let (type_label, type_color) = if app.pr_owned_toggle {
        ("Type: mine (owned)", Color::LightMagenta)
    } else {
        ("Type: ext (not owned)", Color::Gray)
    };
    let type_line = Paragraph::new(Span::styled(type_label, Style::default().fg(type_color)));
    f.render_widget(type_line, chunks[3]);

    let hint = Paragraph::new(Span::styled(
        "Enter confirm · Tab toggle type · Esc cancel",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(hint, chunks[5]);
}

fn draw_directory_picker(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let picker = match &app.dir_picker {
        Some(p) => p,
        None => return,
    };

    let title = match picker.origin {
        DirPickerOrigin::NewTask | DirPickerOrigin::Review => " Select Repos Directory ",
        DirPickerOrigin::RepoSelect | DirPickerOrigin::ReviewRepoSelect => " Select Repository ",
    };

    // Split into header and list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header: show current path
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Path: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            picker.current_dir.display().to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan)),
    );
    f.render_widget(header, chunks[0]);

    // Build list items
    let is_repo_select = picker.is_repo_select_mode();
    let fav_len = picker.favorites_len();
    let mut items: Vec<ListItem> = Vec::new();

    // Favourites section
    if fav_len > 0 {
        // Header line (non-selectable)
        let header_line = Line::from(vec![
            Span::styled(
                format!("── Favourites ({}) ", fav_len),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("─".repeat(40), Style::default().fg(Color::Rgb(60, 60, 60))),
        ]);
        items.push(ListItem::new(header_line));

        for (idx, (repo, count)) in picker.favorite_repos.iter().enumerate() {
            let is_selected = idx == picker.selected_index;
            let style = if is_selected {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if is_selected { "> " } else { "  " };
            let count_str = format!("  ({} tasks)", count);
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{}{}  ", prefix, repo), style),
                Span::styled("[git]", Style::default().fg(Color::LightGreen)),
                Span::styled(count_str, Style::default().fg(Color::DarkGray)),
            ])));
        }

        // Blank separator
        items.push(ListItem::new(Line::from("")));
    }

    // Directory entries
    for (i, name) in picker.entries.iter().enumerate() {
        let flat_index = fav_len + i;
        let is_selected = flat_index == picker.selected_index;
        let kind = picker.entry_kinds.get(i).copied();

        let base_style = if is_selected {
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if is_selected { "> " } else { "  " };

        if is_repo_select {
            let (suffix, suffix_style) = match kind {
                Some(DirKind::GitRepo) => (
                    "  [git]",
                    Style::default().fg(Color::LightGreen),
                ),
                Some(DirKind::MultiRepoParent) => (
                    "  [multi]",
                    Style::default().fg(Color::LightYellow),
                ),
                _ => ("", Style::default()),
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{}{}/", prefix, name), base_style),
                Span::styled(suffix, suffix_style),
            ])));
        } else {
            items.push(ListItem::new(Span::styled(format!("{}{}/", prefix, name), base_style)));
        }
    }

    let help_text = if is_repo_select {
        " j/k: navigate  l/Enter: open/select  h: up  s: select  Esc: cancel "
    } else {
        " j/k: navigate  l/Enter: open  h/Backspace: up  s: select  Esc: cancel "
    };

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                help_text,
                Style::default().fg(Color::DarkGray),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan)),
    );
    f.render_widget(list, chunks[1]);
}

fn humanize_reason(reason: &str) -> &str {
    match reason {
        "assign" => "Assigned",
        "author" => "You authored",
        "comment" => "Comment",
        "ci_activity" => "CI",
        "invitation" => "Invitation",
        "manual" => "Manual",
        "mention" => "Mentioned",
        "review_requested" => "Review requested",
        "security_alert" => "Security alert",
        "state_change" => "State changed",
        "subscribed" => "Subscribed",
        "team_mention" => "Team mention",
        _ => reason,
    }
}

fn short_subject_type(subject_type: &str) -> &str {
    match subject_type {
        "PullRequest" => "PR",
        "Issue" => "Issue",
        "Release" => "Release",
        "Discussion" => "Disc",
        "Commit" => "Commit",
        _ => subject_type,
    }
}

fn relative_time(iso_str: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(iso_str) else {
        return String::new();
    };
    let duration = Utc::now().signed_duration_since(parsed);

    if duration.num_days() > 0 {
        format!("{}d ago", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{}h ago", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{}m ago", duration.num_minutes())
    } else {
        "just now".to_string()
    }
}

fn draw_notifications(f: &mut Frame, app: &App, area: Rect) {
    let count = app.notifications.len();
    let title = format!(" Notifications ({}) ", count);

    if app.notifications.is_empty() {
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title_bottom(clock_title(app));
        let empty_text = if app.gh_notif_first_poll_done {
            "No notifications"
        } else {
            "Fetching notifications..."
        };
        let content = Paragraph::new(empty_text)
            .alignment(Alignment::Center)
            .block(block);
        f.render_widget(content, area);
        return;
    }

    let items: Vec<ListItem> = app
        .notifications
        .iter()
        .enumerate()
        .map(|(i, notif)| {
            let style = if i == app.selected_notif_index {
                Style::default().bg(Color::Rgb(40, 40, 50))
            } else {
                Style::default()
            };

            let meta_color = if notif.unread {
                Color::Rgb(100, 100, 120)
            } else {
                Color::DarkGray
            };

            let title_line = if notif.unread {
                Line::from(vec![
                    Span::styled(" ● ", Style::default().fg(Color::LightCyan)),
                    Span::styled(
                        &notif.title,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled(&notif.title, Style::default().fg(Color::DarkGray)),
                ])
            };

            let time_str = relative_time(&notif.updated_at);
            let mut meta_parts = vec![
                notif.repo_full_name.as_str().to_string(),
                short_subject_type(&notif.subject_type).to_string(),
                humanize_reason(&notif.reason).to_string(),
            ];
            if !time_str.is_empty() {
                meta_parts.push(time_str);
            }
            let meta_text = format!("   {}", meta_parts.join(" · "));
            let meta_line =
                Line::from(Span::styled(meta_text, Style::default().fg(meta_color)));

            ListItem::new(vec![title_line, meta_line, Line::from("")]).style(style)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title_bottom(clock_title(app)),
    );

    f.render_widget(list, area);
}

fn draw_show_prs(f: &mut Frame, app: &mut App, area: Rect) {
    let total_count = app.show_prs_data.issues.len()
        + app.show_prs_data.my_prs.len()
        + app.show_prs_data.review_requests.len();
    let title = format!(" Issues & PRs ({}) ", total_count);

    if total_count == 0 {
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title_bottom(clock_title(app));
        let empty_text = if app.show_prs_first_poll_done {
            "No items"
        } else {
            "Fetching..."
        };
        let content = Paragraph::new(empty_text)
            .alignment(Alignment::Center)
            .block(block);
        f.render_widget(content, area);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut selectable_index: usize = 0;
    let mut visual_index: Option<usize> = None;

    // Helper closure-like sections
    let sections: &[(&str, Color, &[agman::use_cases::GithubItem])] = &[
        ("My Issues", Color::LightYellow, &app.show_prs_data.issues),
        ("My PRs", Color::LightGreen, &app.show_prs_data.my_prs),
        ("Review Requests", Color::LightCyan, &app.show_prs_data.review_requests),
    ];

    for &(section_name, header_color, section_items) in sections {
        // Section header
        let header_text = format!("── {} ({}) ", section_name, section_items.len());
        let remaining = area.width.saturating_sub(header_text.len() as u16 + 2) as usize;
        let fill = "─".repeat(remaining);
        let header_line = Line::from(vec![
            Span::styled(header_text, Style::default().fg(header_color)),
            Span::styled(fill, Style::default().fg(Color::Rgb(60, 60, 60))),
        ]);
        items.push(ListItem::new(vec![header_line]));

        // Items
        for item in section_items {
            let is_selected = selectable_index == app.show_prs_selected;
            if is_selected {
                visual_index = Some(items.len());
            }

            let mut title_spans = vec![
                Span::styled(
                    format!("  #{}", item.number),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("  {}", item.title),
                    Style::default().fg(Color::White),
                ),
            ];
            if item.is_draft {
                title_spans.push(Span::styled(
                    " [draft]",
                    Style::default().fg(Color::Rgb(100, 100, 120)),
                ));
            }
            let title_line = Line::from(title_spans);

            let time_str = relative_time(&item.updated_at);
            let mut meta_parts = vec![
                item.repo_full_name.clone(),
                item.author.clone(),
            ];
            if !time_str.is_empty() {
                meta_parts.push(time_str);
            }
            let meta_text = format!("   {}", meta_parts.join(" · "));
            let meta_line = Line::from(Span::styled(
                meta_text,
                Style::default().fg(Color::Rgb(100, 100, 120)),
            ));

            items.push(ListItem::new(vec![title_line, meta_line, Line::from("")]));
            selectable_index += 1;
        }
    }

    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title_bottom(clock_title(app)),
        )
        .highlight_style(Style::default().bg(Color::Rgb(40, 40, 50)));

    app.show_prs_list_state.select(visual_index);
    f.render_stateful_widget(list, area, &mut app.show_prs_list_state);
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

// ---------------------------------------------------------------------------
// Notes view
// ---------------------------------------------------------------------------

fn draw_notes(f: &mut Frame, app: &mut App, area: Rect) {
    if app.notes_view.is_none() {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    draw_notes_explorer(f, app.notes_view.as_ref().unwrap(), chunks[0]);
    draw_notes_editor(f, app, chunks[1]);
}

fn draw_notes_explorer(f: &mut Frame, nv: &super::app::NotesView, area: Rect) {
    let is_focused = nv.focus == NotesFocus::Explorer;
    let border_color = if is_focused { Color::LightCyan } else { Color::DarkGray };

    // Build title from relative path
    let title = if nv.current_dir == nv.root_dir {
        " Notes ".to_string()
    } else {
        let rel = nv.current_dir.strip_prefix(&nv.root_dir).unwrap_or(&nv.current_dir);
        format!(" Notes/{} ", rel.display())
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    // Reserve space for inline inputs at the bottom
    let inner = block.inner(area);

    // If confirm_delete, show confirmation
    if nv.confirm_delete {
        let entry_name = nv.entries.get(nv.selected_index)
            .map(|e| e.name.as_str())
            .unwrap_or("?");
        let items: Vec<ListItem> = nv.entries.iter().enumerate().map(|(i, entry)| {
            let display = if entry.is_dir {
                format!("  {}/", entry.name)
            } else {
                format!("  {}", entry.name)
            };
            let style = if i == nv.selected_index {
                Style::default().bg(Color::Rgb(40, 40, 50))
            } else {
                Style::default()
            };
            ListItem::new(display).style(style)
        }).collect();

        let list = List::new(items).block(block);
        f.render_widget(list, area);

        // Overlay confirmation at the bottom
        if inner.height > 1 {
            let confirm_area = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let msg = format!(" Delete {}? y/n ", entry_name);
            let confirm = Paragraph::new(msg)
                .style(Style::default().fg(Color::LightRed).bg(Color::Rgb(40, 20, 20)));
            f.render_widget(confirm, confirm_area);
        }
        return;
    }

    // If create_input is active, show it at the bottom
    if let Some((ref input, is_dir)) = nv.create_input {
        let label = if is_dir { "New dir: " } else { "New note: " };

        let items: Vec<ListItem> = nv.entries.iter().enumerate().map(|(i, entry)| {
            let display = if entry.is_dir {
                format!("  {}/", entry.name)
            } else {
                format!("  {}", entry.name)
            };
            let style = if i == nv.selected_index {
                Style::default().bg(Color::Rgb(40, 40, 50))
            } else {
                Style::default()
            };
            ListItem::new(display).style(style)
        }).collect();

        let list = List::new(items).block(block);
        f.render_widget(list, area);

        // Show input at the bottom
        if inner.height > 1 {
            let input_area = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let text = format!("{}{}", label, input.lines()[0]);
            let input_para = Paragraph::new(text)
                .style(Style::default().fg(Color::LightGreen));
            f.render_widget(input_para, input_area);
        }
        return;
    }

    // If rename_input is active, show it inline at the selected position
    if let Some(ref _rename) = nv.rename_input {
        let items: Vec<ListItem> = nv.entries.iter().enumerate().map(|(i, entry)| {
            if i == nv.selected_index {
                let rename_text = nv.rename_input.as_ref().unwrap().lines()[0].clone();
                let display = format!("  {}", rename_text);
                ListItem::new(display).style(Style::default().fg(Color::LightYellow).bg(Color::Rgb(40, 40, 50)))
            } else {
                let display = if entry.is_dir {
                    format!("  {}/", entry.name)
                } else {
                    format!("  {}", entry.name)
                };
                ListItem::new(display).style(Style::default())
            }
        }).collect();

        let list = List::new(items).block(block);
        f.render_widget(list, area);
        return;
    }

    // Normal list rendering
    if nv.entries.is_empty() {
        let empty = Paragraph::new("  (empty)")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = nv.entries.iter().enumerate().map(|(i, entry)| {
        let is_cut = nv.cut_entry.as_ref().is_some_and(|(dir, name)| {
            dir == &nv.current_dir && name == &entry.file_name
        });
        let display = if entry.is_dir {
            format!("  {}/", entry.name)
        } else {
            format!("  {}", entry.name)
        };
        let style = if i == nv.selected_index {
            Style::default().bg(Color::Rgb(40, 40, 50))
        } else {
            Style::default()
        };
        let color = if is_cut {
            Color::DarkGray
        } else if entry.is_dir {
            Color::LightCyan
        } else {
            Color::White
        };
        let style = if is_cut {
            style.fg(color).add_modifier(Modifier::ITALIC)
        } else {
            style.fg(color)
        };
        ListItem::new(display).style(style)
    }).collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_notes_editor(f: &mut Frame, app: &mut App, area: Rect) {
    let nv = match &mut app.notes_view {
        Some(nv) => nv,
        None => return,
    };

    let is_focused = nv.focus == NotesFocus::Editor;
    let border_color = if is_focused {
        match nv.editor.mode() {
            VimMode::Normal => Color::LightCyan,
            VimMode::Insert => Color::LightGreen,
            VimMode::Visual => Color::LightYellow,
            VimMode::Operator(_) => Color::LightMagenta,
        }
    } else {
        Color::DarkGray
    };

    if let Some(ref path) = nv.open_file {
        let file_name = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let modified_indicator = if nv.modified { " [+]" } else { "" };
        let title = format!(" {}{} ", file_name, modified_indicator);

        let mode_indicator = format!(" [{}] ", nv.editor.mode().indicator());
        let block = Block::default()
            .title(title)
            .title_bottom(Line::from(mode_indicator).alignment(Alignment::Left))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let editor_area = block.inner(area);
        f.render_widget(block, area);
        nv.editor.textarea.set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
        f.render_widget(&nv.editor.textarea, editor_area);
    } else {
        let block = Block::default()
            .title(" No file open ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let text = Paragraph::new("Press l or Enter to open a note")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(text, area);
    }
}
