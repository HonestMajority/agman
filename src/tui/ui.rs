use chrono::{Local, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use agman::agent_model::AgentKind;
use agman::use_cases::{self, TelegramHealth};

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::app::{
    AgentActivitySample, App, ArchiveKind, BranchSource, DirKind, DirPickerOrigin, NotesFocus,
    PreviewPane, ProjectPaneFocus, ProjectTaskRow, View, WizardStep,
};
use super::vim::VimMode;

const PROJECT_TASK_COUNT_WIDTH: usize = 8;
const PROJECT_ASSISTANT_COUNT_WIDTH: usize = 10;
const PROJECT_COL_GAP: &str = "    ";

fn vim_mode_color(mode: VimMode) -> Color {
    match mode {
        VimMode::Normal => Color::LightCyan,
        VimMode::Insert => Color::LightGreen,
        VimMode::Visual => Color::LightYellow,
        VimMode::Operator(_) => Color::LightMagenta,
    }
}

fn dim_count_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn active_count_style(active: usize) -> Style {
    if active > 0 {
        Style::default().fg(Color::LightGreen)
    } else {
        dim_count_style()
    }
}

fn push_project_count_cell<'a>(
    spans: &mut Vec<Span<'a>>,
    active: usize,
    total: usize,
    width: usize,
) {
    let active_text = active.to_string();
    let total_text = format!("/{total}");
    let cell_len = active_text.len() + total_text.len();

    if width > cell_len {
        spans.push(Span::raw(" ".repeat(width - cell_len)));
    }
    spans.push(Span::styled(active_text, active_count_style(active)));
    spans.push(Span::styled(total_text, dim_count_style()));
}

fn push_project_blank_cell<'a>(spans: &mut Vec<Span<'a>>, width: usize) {
    spans.push(Span::styled(" ".repeat(width), dim_count_style()));
}

fn truncate_with_ellipsis(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    if value.chars().count() <= width {
        return value.to_string();
    }

    let mut truncated: String = value.chars().take(width.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn clock_title(app: &App) -> Line<'static> {
    let unread_count = app.notifications.iter().filter(|n| n.unread).count();

    let notif_spans = if !app.gh_notif_first_poll_done {
        // Loading state
        vec![Span::styled(
            " GITHUB ... ",
            Style::default().fg(Color::DarkGray),
        )]
    } else if unread_count > 0 {
        // Unread notifications — prominent badge with amber background
        let amber = Color::Rgb(255, 180, 40);
        vec![Span::styled(
            format!(" GITHUB {} ", unread_count),
            Style::default()
                .fg(Color::Black)
                .bg(amber)
                .add_modifier(Modifier::BOLD),
        )]
    } else {
        // Zero unread — hide notification indicator entirely
        vec![]
    };

    let keybase_spans = if !app.keybase_first_poll_done && app.keybase_available {
        vec![Span::styled(
            " KEYBASE ... ",
            Style::default().fg(Color::DarkGray),
        )]
    } else if app.keybase_dm_unread_count > 0 {
        let orange = Color::Rgb(255, 140, 40);
        vec![Span::styled(
            format!(" KEYBASE DM {} ", app.keybase_dm_unread_count),
            Style::default()
                .fg(Color::Black)
                .bg(orange)
                .add_modifier(Modifier::BOLD),
        )]
    } else if app.keybase_channel_unread_count > 0 {
        let cyan = Color::Rgb(0, 180, 216);
        vec![Span::styled(
            format!(" KEYBASE {} ", app.keybase_channel_unread_count),
            Style::default()
                .fg(Color::Black)
                .bg(cyan)
                .add_modifier(Modifier::BOLD),
        )]
    } else {
        vec![]
    };

    let clock_span = Span::styled(
        format!(" {} ", Local::now().format("%H:%M")),
        Style::default().fg(Color::DarkGray),
    );

    let mut spans = notif_spans;
    spans.extend(keybase_spans);
    spans.push(clock_span);

    Line::from(spans).alignment(Alignment::Right)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    // Check if we're showing a modal that should hide the output pane
    let is_modal_view = matches!(
        app.view,
        View::DeleteConfirm
            | View::NewTaskWizard
            | View::DirectoryPicker
            | View::SessionPicker
            | View::ProjectWizard
            | View::ProjectPicker
            | View::ProjectDeleteConfirm
            | View::AgentWizard
            | View::RespawnConfirm
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
        View::ProjectList => draw_project_list(f, app, chunks[0]),
        View::TaskList => draw_project_detail(f, app, chunks[0]),
        View::Preview => draw_preview(f, app, chunks[0]),
        View::DeleteConfirm => {
            draw_project_detail(f, app, chunks[0]);
            draw_delete_confirm(f, app);
        }
        View::NewTaskWizard => {
            draw_project_detail(f, app, chunks[0]);
            draw_wizard(f, app);
        }
        View::DirectoryPicker => {
            draw_project_detail(f, app, chunks[0]);
            draw_directory_picker(f, app);
        }
        View::SessionPicker => {
            draw_preview(f, app, chunks[0]);
            draw_session_picker(f, app);
        }
        View::Notifications => draw_notifications(f, app, chunks[0]),
        View::Notes => draw_notes(f, app, chunks[0]),
        View::ShowPrs => draw_show_prs(f, app, chunks[0]),
        View::Settings => draw_settings(f, app, chunks[0]),
        View::Archive => {
            draw_archive(f, app, chunks[0]);
            if app.archive_preview.is_some() {
                draw_archive_preview(f, app);
            }
        }
        View::ProjectWizard => {
            draw_project_list(f, app, chunks[0]);
            draw_project_wizard(f, app);
        }
        View::ProjectPicker => {
            // Draw the underlying view behind the modal
            if app.project_picker.as_ref().is_some_and(|p| {
                matches!(
                    p.action,
                    super::app::ProjectPickerAction::MigrateAllUnassigned
                )
            }) {
                draw_project_list(f, app, chunks[0]);
            } else {
                draw_project_detail(f, app, chunks[0]);
            }
            draw_project_picker(f, app);
        }
        View::ProjectDeleteConfirm => {
            draw_project_list(f, app, chunks[0]);
            draw_project_delete_confirm(f, app);
        }
        View::AgentWizard => {
            draw_project_detail(f, app, chunks[0]);
            draw_agent_wizard(f, app);
        }
        View::RespawnConfirm => {
            // Draw the underlying view behind the modal
            match app.respawn_confirm_return_view {
                View::ProjectList => draw_project_list(f, app, chunks[0]),
                _ => draw_project_detail(f, app, chunks[0]),
            }
            draw_respawn_confirm(f, app);
        }
    }

    if output_height > 0 {
        draw_output_pane(f, app, chunks[1]);
    }

    draw_status_bar(f, app, chunks[2]);
}

fn render_project_row<'a>(
    app: &'a App,
    project: &'a agman::project::Project,
    i: usize,
    is_held: bool,
    project_width: usize,
    desc_width: usize,
) -> ListItem<'a> {
    let total = app
        .project_task_counts
        .get(&project.meta.name)
        .copied()
        .unwrap_or(0);
    let agent_count = app
        .project_agent_counts
        .get(&project.meta.name)
        .copied()
        .unwrap_or(0);
    let active_agent_count = app
        .project_active_agent_counts
        .get(&project.meta.name)
        .copied()
        .unwrap_or(0);

    let is_selected = i == app.selected_project_index;
    let style = if is_selected {
        Style::default().bg(Color::Rgb(40, 40, 60))
    } else {
        Style::default()
    };

    let name_display = if project.meta.name.chars().count() > project_width {
        truncate_with_ellipsis(&project.meta.name, project_width)
    } else {
        format!("{:<width$}", &project.meta.name, width = project_width)
    };

    let name_style = if is_held {
        Style::default().fg(Color::Gray)
    } else {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };

    let desc_span = if desc_width == 0 {
        Span::raw("")
    } else if project.meta.description.is_empty() {
        Span::styled(
            "No description",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    } else if project.meta.description.chars().count() > desc_width {
        Span::styled(
            truncate_with_ellipsis(&project.meta.description, desc_width),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::styled(
            project.meta.description.as_str(),
            Style::default().fg(Color::DarkGray),
        )
    };

    let is_stalled = app.stalled_targets().contains(&project.meta.name.as_str());

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            if is_selected { "> " } else { "  " },
            Style::default().fg(Color::LightCyan),
        ),
        Span::styled(name_display, name_style),
        Span::raw(PROJECT_COL_GAP),
    ];
    push_project_count_cell(&mut spans, total, total, PROJECT_TASK_COUNT_WIDTH);
    spans.push(Span::raw(PROJECT_COL_GAP));
    push_project_count_cell(
        &mut spans,
        active_agent_count,
        agent_count,
        PROJECT_ASSISTANT_COUNT_WIDTH,
    );
    spans.push(Span::raw(PROJECT_COL_GAP));
    spans.push(desc_span);
    if is_stalled {
        spans.push(Span::styled(
            "  ⚠ stalled",
            Style::default().fg(Color::Yellow),
        ));
    }
    let data_line = Line::from(spans);

    ListItem::new(vec![data_line, Line::from("")]).style(style)
}

fn draw_project_list(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let header = Paragraph::new(Line::from(Span::styled(
        "Projects",
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(header, chunks[0]);

    let area = chunks[1];
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(
                " agman ",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({} projects) ", app.projects.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .title(clock_title(app))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightCyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Check for empty state first
    let has_projects = !app.projects.is_empty() || app.unassigned_task_count > 0;
    if !has_projects {
        let msg = Paragraph::new(
            "No projects. Press 'c' to start Chief of Staff, or create a project via CLI.",
        )
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
        f.render_widget(msg, inner);
        return;
    }

    // Optional CoS stall banner — a dedicated line at the top of the project list.
    let cos_stalled = app.stalled_targets().contains(&"chief-of-staff");

    // Split inner area into (optional CoS banner) + header + list
    let constraints: Vec<Constraint> = if cos_stalled {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ]
    } else {
        vec![Constraint::Length(1), Constraint::Min(0)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let (header_chunk, list_chunk) = if cos_stalled {
        let banner = Paragraph::new(Line::from(Span::styled(
            "⚠ CoS stalled",
            Style::default().fg(Color::Yellow),
        )));
        f.render_widget(banner, chunks[0]);
        (chunks[1], chunks[2])
    } else {
        (chunks[0], chunks[1])
    };

    const MIN_PROJECT_WIDTH: usize = 10;
    const MAX_PROJECT_WIDTH: usize = 25;

    // Calculate dynamic PROJECT column width
    let mut max_name_len = app
        .projects
        .iter()
        .map(|p| p.meta.name.len())
        .max()
        .unwrap_or(MIN_PROJECT_WIDTH);
    if app.unassigned_task_count > 0 {
        max_name_len = max_name_len.max("(unassigned)".len());
    }
    let project_width = max_name_len.clamp(MIN_PROJECT_WIDTH, MAX_PROJECT_WIDTH);

    // Calculate description width
    // Layout: 4 (leading) + project_width + gaps + TASKS + AGENTS
    let fixed_width = 4
        + project_width
        + PROJECT_COL_GAP.len()
        + PROJECT_TASK_COUNT_WIDTH
        + PROJECT_COL_GAP.len()
        + PROJECT_ASSISTANT_COUNT_WIDTH
        + PROJECT_COL_GAP.len();
    let desc_width = (inner.width as usize).saturating_sub(fixed_width);

    // Render header row
    let header_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<width$}", "PROJECT", width = project_width),
            header_style,
        ),
        Span::raw(PROJECT_COL_GAP),
        Span::styled(
            format!("{:>width$}", "TASKS", width = PROJECT_TASK_COUNT_WIDTH),
            header_style,
        ),
        Span::raw(PROJECT_COL_GAP),
        Span::styled(
            format!(
                "{:>width$}",
                "AGENTS",
                width = PROJECT_ASSISTANT_COUNT_WIDTH
            ),
            header_style,
        ),
        Span::raw(PROJECT_COL_GAP),
        Span::styled("DESCRIPTION", header_style),
    ]);
    f.render_widget(Paragraph::new(header), header_chunk);

    // Build list items — partition into active and held projects
    // Render order matches navigation order: active → held → unassigned
    let mut items: Vec<ListItem> = Vec::new();
    let held_count = app.projects.iter().filter(|p| p.meta.held).count();

    // Render active (non-held) projects
    for (i, project) in app.projects.iter().enumerate() {
        if project.meta.held {
            break; // held projects are sorted to the end
        }
        items.push(render_project_row(
            app,
            project,
            i,
            false,
            project_width,
            desc_width,
        ));
    }

    // Render on-hold section header and held projects
    if held_count > 0 {
        let label = format!("── On Hold ({}) ", held_count);
        let fill = (inner.width as usize).saturating_sub(label.len());
        let header_line = Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Rgb(180, 140, 60))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "─".repeat(fill),
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ),
        ]);
        items.push(ListItem::new(header_line));
        items.push(ListItem::new(Line::from("")));

        for (i, project) in app.projects.iter().enumerate() {
            if project.meta.held {
                items.push(render_project_row(
                    app,
                    project,
                    i,
                    true,
                    project_width,
                    desc_width,
                ));
            }
        }
    }

    // Add "(unassigned)" pseudo-entry after held projects (matches navigation order)
    if app.unassigned_task_count > 0 {
        let idx = app.projects.len();
        let is_selected = idx == app.selected_project_index;
        let style = if is_selected {
            Style::default().bg(Color::Rgb(40, 40, 60))
        } else {
            Style::default()
        };

        let name_display = format!("{:<width$}", "(unassigned)", width = project_width);

        let mut line = vec![
            Span::raw("  "),
            Span::styled(
                if is_selected { "> " } else { "  " },
                Style::default().fg(Color::LightCyan),
            ),
            Span::styled(name_display, Style::default().fg(Color::DarkGray)),
            Span::raw(PROJECT_COL_GAP),
        ];
        push_project_count_cell(
            &mut line,
            app.unassigned_task_count,
            app.unassigned_task_count,
            PROJECT_TASK_COUNT_WIDTH,
        );
        line.push(Span::raw(PROJECT_COL_GAP));
        push_project_blank_cell(&mut line, PROJECT_ASSISTANT_COUNT_WIDTH);
        items.push(ListItem::new(vec![Line::from(line), Line::from("")]).style(style));
    }

    let list = List::new(items);
    f.render_widget(list, list_chunk);
}

fn draw_project_wizard(f: &mut Frame, app: &mut App) {
    let area = centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);

    let wizard = match &mut app.project_wizard {
        Some(w) => w,
        None => return,
    };

    let has_error = wizard.error_message.is_some();
    let footer_height = if has_error { 2 } else { 1 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),             // name field
            Constraint::Min(5),                // description field
            Constraint::Length(footer_height), // help/error
        ])
        .split(area);

    // Name field
    let name_focused = !wizard.description_focus;
    let name_border_color = if name_focused {
        Color::LightCyan
    } else {
        Color::DarkGray
    };
    wizard.name_editor.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(name_border_color))
            .title(Span::styled(
                " New Project — Name ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    wizard.name_editor.set_cursor_style(if name_focused {
        Style::default().bg(Color::LightCyan).fg(Color::Black)
    } else {
        Style::default()
    });
    f.render_widget(&wizard.name_editor, chunks[0]);

    // Description field
    let desc_focused = wizard.description_focus;
    let desc_border_color = if desc_focused {
        Color::LightCyan
    } else {
        Color::DarkGray
    };
    let mode = wizard.description_editor.mode();
    let mode_indicator = if desc_focused {
        format!(" [{}] ", mode.indicator())
    } else {
        String::new()
    };
    wizard.description_editor.textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(desc_border_color))
            .title(Span::styled(
                format!(" Description{mode_indicator}"),
                Style::default().fg(Color::DarkGray),
            )),
    );
    if desc_focused {
        wizard
            .description_editor
            .textarea
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
    }
    f.render_widget(&wizard.description_editor.textarea, chunks[1]);

    // Footer: error or help text
    let footer_spans = if let Some(ref err) = wizard.error_message {
        vec![Span::styled(
            err.clone(),
            Style::default().fg(Color::LightRed),
        )]
    } else {
        vec![
            Span::styled("Tab", Style::default().fg(Color::LightCyan)),
            Span::styled(" switch field  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
            Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::LightCyan)),
            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
        ]
    };
    let footer = Paragraph::new(Line::from(footer_spans)).alignment(Alignment::Center);
    f.render_widget(footer, chunks[2]);
}

fn draw_agent_wizard(f: &mut Frame, app: &mut App) {
    use super::app::{AgentWizardKind, AgentWizardStep};

    // Reviewer worktrees can grow tall — give the modal more room than the
    // legacy researcher wizard.
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let harness_kind = app.config.harness_kind();
    let wizard = match &mut app.agent_wizard {
        Some(w) => w,
        None => return,
    };

    let kind_label = match wizard.kind {
        AgentWizardKind::Researcher => "Researcher",
        AgentWizardKind::Operator => "Operator",
        AgentWizardKind::Reviewer => "Reviewer",
        AgentWizardKind::Tester => "Tester",
    };
    let title = format!(" New {} — {} ", kind_label, wizard.project);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let has_error = wizard.error_message.is_some();
    let footer_height = if has_error { 2 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(footer_height)])
        .split(inner);

    match wizard.step {
        AgentWizardStep::Kind => draw_agent_wizard_kind(f, wizard, chunks[0]),
        AgentWizardStep::Name => draw_agent_wizard_name(f, wizard, chunks[0]),
        AgentWizardStep::Worktrees => draw_agent_wizard_worktrees(f, wizard, chunks[0]),
        AgentWizardStep::Capabilities => {
            draw_agent_wizard_capabilities(f, harness_kind, wizard, chunks[0])
        }
        AgentWizardStep::Description => draw_agent_wizard_description(f, wizard, chunks[0]),
    }

    let footer_spans: Vec<Span> = if let Some(ref err) = wizard.error_message {
        vec![Span::styled(
            err.clone(),
            Style::default().fg(Color::LightRed),
        )]
    } else {
        match wizard.step {
            AgentWizardStep::Kind => vec![
                Span::styled("←/→", Style::default().fg(Color::LightCyan)),
                Span::styled(" switch  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ],
            AgentWizardStep::Name => vec![
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ],
            AgentWizardStep::Worktrees => vec![
                Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                Span::styled(" next field  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Ctrl+A", Style::default().fg(Color::LightGreen)),
                Span::styled(" add row  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Ctrl+D", Style::default().fg(Color::LightRed)),
                Span::styled(" remove row  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" back", Style::default().fg(Color::DarkGray)),
            ],
            AgentWizardStep::Capabilities => vec![
                Span::styled("Space", Style::default().fg(Color::LightCyan)),
                Span::styled(" toggle  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" back", Style::default().fg(Color::DarkGray)),
            ],
            AgentWizardStep::Description => vec![
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" back", Style::default().fg(Color::DarkGray)),
            ],
        }
    };
    let footer = Paragraph::new(Line::from(footer_spans)).alignment(Alignment::Center);
    f.render_widget(footer, chunks[1]);
}

fn draw_agent_wizard_kind(f: &mut Frame, wizard: &super::app::AgentWizard, area: Rect) {
    use super::app::AgentWizardKind;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);

    let researcher_selected = matches!(wizard.kind, AgentWizardKind::Researcher);
    let researcher_style = if researcher_selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let researcher =
        Paragraph::new(Line::from(vec![Span::styled(
            " Researcher  —  long-lived investigator scoped to one task/branch ",
            researcher_style,
        )]))
        .block(Block::default().borders(Borders::ALL).border_style(
            Style::default().fg(if researcher_selected {
                Color::LightCyan
            } else {
                Color::DarkGray
            }),
        ));
    f.render_widget(researcher, chunks[0]);

    let operator_selected = matches!(wizard.kind, AgentWizardKind::Operator);
    let operator_style = if operator_selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let operator =
        Paragraph::new(Line::from(vec![Span::styled(
            " Operator    —  takes action in external systems and reports back ",
            operator_style,
        )]))
        .block(Block::default().borders(Borders::ALL).border_style(
            Style::default().fg(if operator_selected {
                Color::LightCyan
            } else {
                Color::DarkGray
            }),
        ));
    f.render_widget(operator, chunks[3]);

    let reviewer_selected = matches!(wizard.kind, AgentWizardKind::Reviewer);
    let reviewer_style = if reviewer_selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let reviewer =
        Paragraph::new(Line::from(vec![Span::styled(
            " Reviewer    —  scoped to one or more (repo, branch) worktrees ",
            reviewer_style,
        )]))
        .block(Block::default().borders(Borders::ALL).border_style(
            Style::default().fg(if reviewer_selected {
                Color::LightCyan
            } else {
                Color::DarkGray
            }),
        ));
    f.render_widget(reviewer, chunks[1]);

    let tester_selected = matches!(wizard.kind, AgentWizardKind::Tester);
    let tester_style = if tester_selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let tester =
        Paragraph::new(Line::from(vec![Span::styled(
            " Tester      —  runs tests and exercises behavior in worktrees ",
            tester_style,
        )]))
        .block(Block::default().borders(Borders::ALL).border_style(
            Style::default().fg(if tester_selected {
                Color::LightCyan
            } else {
                Color::DarkGray
            }),
        ));
    f.render_widget(tester, chunks[2]);
}

fn draw_agent_wizard_name(f: &mut Frame, wizard: &mut super::app::AgentWizard, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3)])
        .split(area);
    wizard.name_editor.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan))
            .title(Span::styled(
                " Name ",
                Style::default().fg(Color::LightCyan),
            )),
    );
    wizard
        .name_editor
        .set_cursor_style(Style::default().bg(Color::LightCyan).fg(Color::Black));
    f.render_widget(&wizard.name_editor, chunks[0]);
}

fn draw_agent_wizard_worktrees(f: &mut Frame, wizard: &mut super::app::AgentWizard, area: Rect) {
    // One block per row, each split horizontally into repo and branch fields.
    // Borders highlight the focused field; the selected row is the only one
    // with a coloured border at all.
    let row_count = wizard.worktree_rows.len();
    let constraints: Vec<Constraint> = (0..row_count).map(|_| Constraint::Length(3)).collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let selected_row = wizard.selected_row;
    for (i, row) in wizard.worktree_rows.iter_mut().enumerate() {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(chunks[i]);

        let row_is_selected = i == selected_row;
        let repo_focused = row_is_selected && !row.branch_focus;
        let branch_focused = row_is_selected && row.branch_focus;

        row.repo_editor.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if repo_focused {
                    Color::LightCyan
                } else if row_is_selected {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
                .title(Span::styled(
                    format!(" repo #{} ", i + 1),
                    Style::default().fg(if row_is_selected {
                        Color::LightCyan
                    } else {
                        Color::DarkGray
                    }),
                )),
        );
        row.repo_editor.set_cursor_style(if repo_focused {
            Style::default().bg(Color::LightCyan).fg(Color::Black)
        } else {
            Style::default()
        });
        f.render_widget(&row.repo_editor, cols[0]);

        row.branch_editor.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if branch_focused {
                    Color::LightCyan
                } else if row_is_selected {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
                .title(Span::styled(
                    " branch ",
                    Style::default().fg(if row_is_selected {
                        Color::LightCyan
                    } else {
                        Color::DarkGray
                    }),
                )),
        );
        row.branch_editor.set_cursor_style(if branch_focused {
            Style::default().bg(Color::LightCyan).fg(Color::Black)
        } else {
            Style::default()
        });
        f.render_widget(&row.branch_editor, cols[1]);
    }
}

fn draw_agent_wizard_capabilities(
    f: &mut Frame,
    harness_kind: agman::harness::HarnessKind,
    wizard: &super::app::AgentWizard,
    area: Rect,
) {
    let unsupported = matches!(
        harness_kind,
        agman::harness::HarnessKind::Goose | agman::harness::HarnessKind::Pi
    );
    let label = if wizard.browser_capability {
        " Browser: on "
    } else {
        " Browser: off "
    };
    let mut spans = vec![Span::styled(
        label,
        if unsupported {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        },
    )];
    if unsupported {
        spans.push(Span::styled(
            format!(" (not supported on {harness_kind})"),
            Style::default().fg(Color::Yellow),
        ));
    }

    let paragraph = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if unsupported {
                Color::DarkGray
            } else {
                Color::LightCyan
            }))
            .title(Span::styled(
                " Capabilities ",
                Style::default().fg(Color::LightCyan),
            )),
    );
    f.render_widget(paragraph, area);
}

fn draw_agent_wizard_description(f: &mut Frame, wizard: &mut super::app::AgentWizard, area: Rect) {
    let mode = wizard.description_editor.mode();
    let mode_indicator = format!(" [{}] ", mode.indicator());
    wizard.description_editor.textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan))
            .title(Span::styled(
                format!(" Description{mode_indicator}"),
                Style::default().fg(Color::LightCyan),
            )),
    );
    wizard
        .description_editor
        .textarea
        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
    f.render_widget(&wizard.description_editor.textarea, area);
}

fn draw_project_picker(f: &mut Frame, app: &mut App) {
    let Some(picker) = &app.project_picker else {
        return;
    };

    let title = match &picker.action {
        super::app::ProjectPickerAction::MigrateAllUnassigned => "Migrate All Unassigned Tasks To",
    };

    let area = centered_rect(40, 50, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = picker
        .projects
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let style = if i == picker.selected {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(40, 40, 60))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Span::styled(format!("  {name}"), style))
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn draw_project_detail(f: &mut Frame, app: &App, area: Rect) {
    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);

    let header = Paragraph::new(Line::from(Span::styled(
        app.current_project.as_deref().unwrap_or("Project"),
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(header, panes[0]);

    draw_project_agents_pane(
        f,
        app,
        panes[1],
        app.project_pane_focus == ProjectPaneFocus::Agents,
    );
    draw_tasks_pane(
        f,
        app,
        panes[2],
        app.project_pane_focus == ProjectPaneFocus::Tasks,
    );
}

fn draw_tasks_pane(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    // Create the outer block first
    let border_style = if focused {
        Style::default().fg(Color::LightCyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_style = if focused {
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" Tasks ", title_style),
            Span::styled(
                format!("({} tasks) ", app.tasks.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .title(clock_title(app))
        .borders(Borders::ALL)
        .border_style(border_style);

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
    const UPDATED_WIDTH: usize = 10;
    const COL_GAP: &str = "    "; // 4 spaces between columns

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

    let repo_width = max_repo_len.clamp(MIN_REPO_WIDTH, MAX_REPO_WIDTH);

    // Scan tasks for longest branch name.
    let max_branch_len = app
        .tasks
        .iter()
        .map(|t| t.meta.branch_name.len())
        .max()
        .unwrap_or(MIN_BRANCH_WIDTH);

    let branch_width = max_branch_len.max(MIN_BRANCH_WIDTH);

    // Compute fixed width from actual components:
    // padding(5) + col_gaps(3*4=12) + repo + pr + updated
    let fixed_cols_width = (5 + 12 + repo_width + PR_WIDTH + UPDATED_WIDTH) as u16;

    let available_width = inner.width.saturating_sub(fixed_cols_width) as usize;

    // Cap branch width to available space
    let branch_width = branch_width.min(available_width.max(MIN_BRANCH_WIDTH));

    // Render header - columns: space(5) + repo + gap + branch + gap + PR + gap + updated
    let header = Line::from(vec![
        Span::raw("     "),
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
            "UPDATED",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    if app.tasks.is_empty() {
        let text = Paragraph::new("No tasks")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(text, chunks[1]);
        return;
    }

    // Build task list with attached agents as child rows.
    let mut items: Vec<ListItem> = Vec::new();

    for (row_index, row) in app.project_task_rows().iter().enumerate() {
        let ProjectTaskRow::Task {
            task_index: _,
            task,
        } = row
        else {
            if let ProjectTaskRow::Agent { agent, .. } = row {
                let is_selected = focused && row_index == app.selected_index;
                let (status, status_icon, status_color) = agent_runtime_status(app, agent);
                let text_style = if is_selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let row_style = if is_selected {
                    Style::default().bg(Color::Rgb(40, 40, 50))
                } else {
                    Style::default()
                };
                let label = format!("{} {}", agent_kind_label(&agent.meta.kind), agent.meta.name);
                let display_label = truncate_to_width(&label, repo_width + branch_width + 4);
                let line = Line::from(vec![
                    Span::raw("       "),
                    Span::styled(status_icon, Style::default().fg(status_color)),
                    Span::raw(" "),
                    Span::styled(
                        format!(
                            "{:<width$}",
                            display_label,
                            width = repo_width + branch_width + 4
                        ),
                        text_style,
                    ),
                    Span::raw(COL_GAP),
                    Span::styled(
                        format!("{:<width$}", status, width = PR_WIDTH),
                        Style::default().fg(status_color),
                    ),
                    Span::raw(COL_GAP),
                    Span::styled(
                        time_since_datetime(&agent.meta.created_at),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                items.push(ListItem::new(line).style(row_style));
            }
            continue;
        };
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

        let full_branch = task.meta.branch_name.clone();

        // Truncate branch if needed, with ellipsis
        let display_branch = if full_branch.len() > branch_width {
            format!("{}…", &full_branch[..branch_width.saturating_sub(1)])
        } else {
            full_branch.clone()
        };

        let is_selected = focused && row_index == app.selected_index;
        let text_color = if is_selected {
            Color::White
        } else {
            Color::Rgb(140, 140, 140)
        };

        // Build PR display string and truncate if needed
        let pr_display = task
            .meta
            .linked_pr
            .as_ref()
            .map(|pr| {
                if !pr.owned {
                    format!(
                        "#{:>5} ({})",
                        pr.number,
                        pr.author.as_deref().unwrap_or("ext")
                    )
                } else {
                    format!("#{:>5} mine", pr.number)
                }
            })
            .unwrap_or_default();
        let pr_display = if pr_display.len() > PR_WIDTH {
            format!("{}…", &pr_display[..PR_WIDTH.saturating_sub(1)])
        } else {
            pr_display
        };

        let line = Line::from(vec![
            Span::raw("     "),
            Span::styled(
                format!("{:<width$}", display_repo, width = repo_width),
                if is_selected {
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
                if is_selected {
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
                    } else {
                        Style::default().fg(Color::LightMagenta)
                    }
                } else {
                    Style::default()
                },
            ),
            Span::raw(COL_GAP),
            Span::styled(
                task.time_since_update(),
                Style::default().fg(Color::DarkGray),
            ),
        ]);

        let style = if is_selected {
            Style::default().bg(Color::Rgb(40, 40, 50))
        } else {
            Style::default()
        };

        items.push(ListItem::new(line).style(style));
    }

    let list = List::new(items);
    f.render_widget(list, chunks[1]);
}

fn draw_project_agents_pane(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let border_style = if focused {
        Style::default().fg(Color::LightCyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_style = if focused {
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" Agents ", title_style),
            Span::styled(
                format!("({} agents) ", app.agents.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.agents.is_empty() {
        let text = Paragraph::new("No agents")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    const MIN_NAME_WIDTH: usize = 4;
    const MAX_NAME_WIDTH: usize = 56;
    const TYPE_WIDTH: usize = 10;
    const STATUS_WIDTH: usize = 10;
    const CREATED_WIDTH: usize = 10;
    const COL_GAP: &str = "   ";

    let max_name_len = app
        .agents
        .iter()
        .map(|a| a.meta.name.len())
        .max()
        .unwrap_or(MIN_NAME_WIDTH);
    let fixed_width = 4 + (COL_GAP.len() * 3) + TYPE_WIDTH + STATUS_WIDTH + CREATED_WIDTH;
    let available_name_width = (inner.width as usize)
        .saturating_sub(fixed_width)
        .clamp(MIN_NAME_WIDTH, MAX_NAME_WIDTH);
    let name_width = max_name_len.clamp(MIN_NAME_WIDTH, available_name_width);

    let header_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<width$}", "NAME", width = name_width),
            header_style,
        ),
        Span::raw(COL_GAP),
        Span::styled(
            format!("{:<width$}", "TYPE", width = TYPE_WIDTH),
            header_style,
        ),
        Span::raw(COL_GAP),
        Span::styled(
            format!("{:<width$}", "STATUS", width = STATUS_WIDTH),
            header_style,
        ),
        Span::raw(COL_GAP),
        Span::styled(
            format!("{:<width$}", "CREATED", width = CREATED_WIDTH),
            header_style,
        ),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    let items: Vec<ListItem> = app
        .agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let is_selected = focused && i == app.agent_list_index;
            let (status, status_icon, status_color) = agent_runtime_status(app, agent);
            let row_style = if is_selected {
                Style::default().bg(Color::Rgb(40, 40, 50))
            } else {
                Style::default()
            };
            let text_style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(status_icon, Style::default().fg(status_color)),
                Span::raw("  "),
                Span::styled(
                    format!(
                        "{:<width$}",
                        truncate_to_width(&agent.meta.name, name_width),
                        width = name_width
                    ),
                    text_style,
                ),
                Span::raw(COL_GAP),
                Span::styled(
                    format!(
                        "{:<width$}",
                        agent_kind_label(&agent.meta.kind),
                        width = TYPE_WIDTH
                    ),
                    Style::default().fg(Color::LightMagenta),
                ),
                Span::raw(COL_GAP),
                Span::styled(
                    format!("{:<width$}", status, width = STATUS_WIDTH),
                    Style::default().fg(status_color),
                ),
                Span::raw(COL_GAP),
                Span::styled(
                    format!(
                        "{:<width$}",
                        time_since_datetime(&agent.meta.created_at),
                        width = CREATED_WIDTH
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);

            ListItem::new(line).style(row_style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, chunks[1]);
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

fn agent_session_name(agent: &agman::agent_model::AgentRecord) -> String {
    use agman::config::Config;

    match &agent.meta.kind {
        AgentKind::Engineer => Config::engineer_tmux_session(&agent.meta.project, &agent.meta.name),
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

const ASSISTANT_WORKING_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkingIdle {
    Working,
    Idle,
}

pub(super) fn classify_agent_status(
    now: Instant,
    sample: Option<&AgentActivitySample>,
) -> WorkingIdle {
    let Some(sample) = sample else {
        return WorkingIdle::Idle;
    };
    if !sample.query_ok || sample.pane_dead || sample.foreground_command_is_shell() {
        return WorkingIdle::Idle;
    }
    if sample.activity_age(now) <= ASSISTANT_WORKING_GRACE {
        WorkingIdle::Working
    } else {
        WorkingIdle::Idle
    }
}

fn agent_runtime_status(
    app: &App,
    agent: &agman::agent_model::AgentRecord,
) -> (&'static str, &'static str, Color) {
    let session_name = agent_session_name(agent);
    match classify_agent_status(Instant::now(), app.agent_activity_sample(&session_name)) {
        WorkingIdle::Working => ("working", "●", Color::LightGreen),
        WorkingIdle::Idle => ("idle", "○", Color::DarkGray),
    }
}

fn time_since_datetime(timestamp: &chrono::DateTime<Utc>) -> String {
    let duration = Utc::now().signed_duration_since(*timestamp);

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

fn truncate_to_width(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if value.len() > width {
        format!("{}…", &value[..width.saturating_sub(1)])
    } else {
        value.to_string()
    }
}

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Task info header
    if let Some(task) = app.selected_task() {
        let header_spans = vec![
            Span::styled("Task: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                task.meta.task_id(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ];

        let header = Paragraph::new(Line::from(header_spans)).block(
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

fn draw_logs_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.preview_pane == PreviewPane::Logs;

    let (title, title_style, border_color) = if is_focused {
        let mode = app.logs_editor.mode();
        let color = vim_mode_color(mode);
        (
            format!(" Logs [{}] ", mode.indicator()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
            color,
        )
    } else {
        (
            " Logs ".to_string(),
            Style::default().fg(Color::DarkGray),
            Color::DarkGray,
        )
    };

    app.logs_editor.textarea.set_block(
        Block::default()
            .title(Span::styled(title, title_style))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    app.logs_editor
        .textarea
        .set_cursor_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(&app.logs_editor.textarea, area);
}

fn draw_notes_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.preview_pane == PreviewPane::Notes;

    let (title, title_style, border_color) = if is_focused {
        let mode = app.notes_editor.mode();
        let color = vim_mode_color(mode);
        (
            format!(" Notes [{}] ", mode.indicator()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
            color,
        )
    } else {
        (
            " Notes ".to_string(),
            Style::default().fg(Color::DarkGray),
            Color::DarkGray,
        )
    };

    app.notes_editor.textarea.set_block(
        Block::default()
            .title(Span::styled(title, title_style))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );

    let cursor_style = if app.notes_editing {
        Style::default().bg(Color::White).fg(Color::Black)
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    };
    app.notes_editor.textarea.set_cursor_style(cursor_style);

    f.render_widget(&app.notes_editor.textarea, area);
}

fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(52, 28, f.area());

    f.render_widget(Clear, area);

    let (question, subject) = match app.project_pane_focus {
        ProjectPaneFocus::Tasks => (
            "Archive this task?",
            app.selected_task()
                .map(|t| t.meta.task_id())
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        ProjectPaneFocus::Agents => (
            "Archive this agent?",
            app.agents
                .get(app.agent_list_index)
                .map(|agent| format!("{}--{}", agent.meta.project, agent.meta.name))
                .unwrap_or_else(|| "unknown".to_string()),
        ),
    };

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {question}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {subject}"),
            Style::default().fg(Color::LightCyan),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  This moves the item to the archive. Permanent delete remains",
            Style::default().fg(Color::LightBlue),
        )),
        Line::from(Span::styled(
            "  available from the archive view.",
            Style::default().fg(Color::LightBlue),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  [Enter] archive   [Esc] cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let popup = Paragraph::new(text).block(
        Block::default()
            .title(Span::styled(
                " Archive ",
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightBlue)),
    );

    f.render_widget(popup, area);
}

fn draw_project_delete_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 30, f.area());

    f.render_widget(Clear, area);

    let project_name = app.project_to_delete.as_deref().unwrap_or("unknown");

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  Delete project '{}'?", project_name),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  All tasks will be archived (not permanently deleted).",
            Style::default().fg(Color::LightBlue),
        )),
        Line::from(Span::styled(
            "  Branches and task files are preserved.",
            Style::default().fg(Color::LightBlue),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  [Enter] confirm   [Esc] cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let popup = Paragraph::new(text).block(
        Block::default()
            .title(Span::styled(
                " Delete Project ",
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightRed)),
    );

    f.render_widget(popup, area);
}

fn draw_respawn_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 35, f.area());

    f.render_widget(Clear, area);

    let sel = app.respawn_confirm_index;

    let option0_style = if sel == 0 {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(30, 40, 60))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let option1_style = if sel == 1 {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(30, 40, 60))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let prefix0 = if sel == 0 { "▸ " } else { "  " };
    let prefix1 = if sel == 1 { "▸ " } else { "  " };

    let (title, text) = if app.respawn_confirm_is_chief_of_staff {
        (
            " Respawn Chief of Staff ",
            vec![
                Line::from(""),
                Line::from(Span::styled(format!("{prefix0}CoS only"), option0_style)),
                Line::from(""),
                Line::from(Span::styled(
                    format!("{prefix1}CoS + all PMs"),
                    option1_style,
                )),
            ],
        )
    } else {
        let target = app.respawn_confirm_target.as_deref().unwrap_or("unknown");
        (
            " Respawn PM ",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  Respawn PM for '{target}'?"),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(format!("{prefix0}Respawn"), option0_style)),
                Line::from(""),
                Line::from(Span::styled(format!("{prefix1}Cancel"), option1_style)),
            ],
        )
    };

    let popup = Paragraph::new(text).block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightMagenta)),
    );

    f.render_widget(popup, area);
}

fn draw_output_pane(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .output_log
        .iter()
        .map(|line| {
            let lower = line.to_lowercase();
            let is_error =
                lower.contains("error") || lower.contains("failed") || lower.contains("[stderr]");
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

/// Status-bar Telegram health indicator: `tg ●` colored by [`TelegramHealth`].
/// Returns an empty vec when the bot is not configured (so the indicator
/// disappears entirely rather than rendering a neutral dot).
fn telegram_health_spans(app: &App) -> Vec<Span<'static>> {
    let configured = app.telegram.is_some();
    let heartbeat = app
        .telegram
        .as_ref()
        .map(|h| h.heartbeat.load(Ordering::Relaxed))
        .filter(|v| *v != 0);
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let health = use_cases::classify_telegram_health(heartbeat, now_epoch, configured);
    let dot_color = match health {
        TelegramHealth::Disabled => return vec![],
        TelegramHealth::Healthy => Color::LightGreen,
        TelegramHealth::Stale => Color::LightYellow,
        TelegramHealth::Dead | TelegramHealth::NeverPolled => Color::LightRed,
    };
    vec![
        Span::styled("tg ", Style::default().fg(Color::DarkGray)),
        Span::styled("●", Style::default().fg(dot_color)),
    ]
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.view {
        View::ProjectList => {
            let mut spans = vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
                Span::styled("n", Style::default().fg(Color::LightGreen)),
                Span::styled(" new  ", Style::default().fg(Color::DarkGray)),
                Span::styled("c", Style::default().fg(Color::LightYellow)),
                Span::styled(" CoS chat  ", Style::default().fg(Color::DarkGray)),
            ];
            // Show migrate hint when (unassigned) is selected
            let is_unassigned =
                app.selected_project_index >= app.projects.len() && app.unassigned_task_count > 0;
            if is_unassigned {
                spans.extend([
                    Span::styled("m", Style::default().fg(Color::LightMagenta)),
                    Span::styled(" migrate  ", Style::default().fg(Color::DarkGray)),
                ]);
            }
            // Show delete and hold hints when a real project is selected
            if app.selected_project_index < app.projects.len() {
                spans.extend([
                    Span::styled("d", Style::default().fg(Color::LightRed)),
                    Span::styled(" delete  ", Style::default().fg(Color::DarkGray)),
                ]);
                let hold_label = if app.projects[app.selected_project_index].meta.held {
                    " unhold  "
                } else {
                    " hold  "
                };
                spans.extend([
                    Span::styled("h", Style::default().fg(Color::LightYellow)),
                    Span::styled(hold_label, Style::default().fg(Color::DarkGray)),
                ]);
            }
            spans.extend([
                Span::styled("e", Style::default().fg(Color::LightMagenta)),
                Span::styled(" respawn  ", Style::default().fg(Color::DarkGray)),
                Span::styled("o", Style::default().fg(Color::LightYellow)),
                Span::styled(" notes  ", Style::default().fg(Color::DarkGray)),
            ]);
            let unread_count = app.notifications.iter().filter(|n| n.unread).count();
            let inbox_label = if unread_count > 0 {
                format!(" inbox({})  ", unread_count)
            } else if !app.gh_notif_first_poll_done {
                " inbox(...)  ".to_string()
            } else {
                " inbox  ".to_string()
            };
            spans.extend([
                Span::styled("i", Style::default().fg(Color::LightYellow)),
                Span::styled(inbox_label, Style::default().fg(Color::DarkGray)),
                Span::styled("p", Style::default().fg(Color::LightYellow)),
                Span::styled(" prs  ", Style::default().fg(Color::DarkGray)),
                Span::styled(",", Style::default().fg(Color::LightYellow)),
                Span::styled(" settings  ", Style::default().fg(Color::DarkGray)),
            ]);
            spans
        }
        View::TaskList => {
            let mut spans = vec![
                Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                Span::styled(" pane  ", Style::default().fg(Color::DarkGray)),
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("n", Style::default().fg(Color::LightGreen)),
                Span::styled(" new  ", Style::default().fg(Color::DarkGray)),
            ];
            if app.project_pane_focus == ProjectPaneFocus::Agents {
                if app
                    .current_project
                    .as_deref()
                    .is_some_and(|p| p != "(unassigned)")
                {
                    spans.extend([
                        Span::styled("c", Style::default().fg(Color::LightYellow)),
                        Span::styled(" PM chat  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans.extend([
                    Span::styled("enter", Style::default().fg(Color::LightGreen)),
                    Span::styled(" attach  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("d", Style::default().fg(Color::LightRed)),
                    Span::styled(" archive  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("z", Style::default().fg(Color::LightYellow)),
                    Span::styled(" archived  ", Style::default().fg(Color::DarkGray)),
                ]);
                if app
                    .current_project
                    .as_deref()
                    .is_some_and(|p| p != "(unassigned)")
                {
                    spans.extend([
                        Span::styled("e", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" respawn  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                if app.current_project.is_some() {
                    spans.extend([
                        Span::styled("q", Style::default().fg(Color::LightCyan)),
                        Span::styled(" back", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans
            } else {
                // Show PM chat hint when in a project scope
                if app
                    .current_project
                    .as_deref()
                    .is_some_and(|p| p != "(unassigned)")
                {
                    spans.extend([
                        Span::styled("c", Style::default().fg(Color::LightYellow)),
                        Span::styled(" PM chat  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                if let Some(task) = app.selected_task() {
                    if task.meta.linked_pr.is_some() {
                        spans.push(Span::styled("o", Style::default().fg(Color::LightYellow)));
                        spans.push(Span::styled(
                            " open pr  ",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    // Task-selected hints (always shown when a task is selected)
                    spans.extend([
                        Span::styled("r", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" rerun  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("d", Style::default().fg(Color::LightRed)),
                        Span::styled(" archive  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                if app
                    .current_project
                    .as_deref()
                    .is_some_and(|p| p != "(unassigned)")
                {
                    spans.extend([
                        Span::styled("e", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" respawn  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans.extend([
                    Span::styled("z", Style::default().fg(Color::LightYellow)),
                    Span::styled(" archived  ", Style::default().fg(Color::DarkGray)),
                ]);
                if app.current_project.is_some() {
                    spans.extend([
                        Span::styled("q", Style::default().fg(Color::LightCyan)),
                        Span::styled(" back", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans
            }
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
                ];
                if let Some(task) = app.selected_task() {
                    if task.meta.linked_pr.is_some() {
                        spans.push(Span::styled("o", Style::default().fg(Color::LightYellow)));
                        spans.push(Span::styled(
                            " open pr  ",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    // Task-selected hints (always shown when a task is selected)
                    spans.extend([
                        Span::styled("r", Style::default().fg(Color::LightMagenta)),
                        Span::styled(" rerun  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans.extend([
                    Span::styled("Enter", Style::default().fg(Color::LightCyan)),
                    Span::styled(" attach  ", Style::default().fg(Color::DarkGray)),
                ]);
                spans.extend([
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]);
                spans
            }
        }
        View::DeleteConfirm => {
            vec![
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" archive  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc/q", Style::default().fg(Color::LightRed)),
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
        View::SessionPicker => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
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
                Span::styled("r", Style::default().fg(Color::LightYellow)),
                Span::styled(" refresh  ", Style::default().fg(Color::DarkGray)),
            ];
            spans.extend([
                Span::styled("q", Style::default().fg(Color::LightCyan)),
                Span::styled(" back", Style::default().fg(Color::DarkGray)),
            ]);
            spans
        }
        View::Notes => {
            let is_editor = app
                .notes_view
                .as_ref()
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
                spans.extend([
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]);
                spans
            }
        }
        View::Settings => {
            if app.settings_editing {
                vec![
                    Span::styled("Enter", Style::default().fg(Color::LightCyan)),
                    Span::styled(" save  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                    Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
                ]
            } else {
                let mut spans = vec![
                    Span::styled("h/l", Style::default().fg(Color::LightCyan)),
                    Span::styled(" adjust  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                ];
                if app.settings_selected >= 2 {
                    spans.extend([
                        Span::styled("Enter", Style::default().fg(Color::LightCyan)),
                        Span::styled(" edit  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans.extend([
                    Span::styled("q", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]);
                spans
            }
        }
        View::Archive => {
            if app.archive_preview.is_some() {
                let mut spans = vec![
                    Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                    Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
                ];
                if app.archive_kind == ArchiveKind::Tasks {
                    spans.extend([
                        Span::styled("s", Style::default().fg(Color::LightGreen)),
                        Span::styled(
                            {
                                let filtered = app.archive_filtered_indices();
                                if filtered
                                    .get(app.archive_selected)
                                    .and_then(|&i| app.archive_tasks.get(i))
                                    .map(|(t, _)| t.meta.saved)
                                    .unwrap_or(false)
                                {
                                    " unsave  "
                                } else {
                                    " save  "
                                }
                            },
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled("n", Style::default().fg(Color::LightCyan)),
                        Span::styled(" new-from  ", Style::default().fg(Color::DarkGray)),
                    ]);
                } else {
                    spans.extend([
                        Span::styled("n", Style::default().fg(Color::LightCyan)),
                        Span::styled(" restore  ", Style::default().fg(Color::DarkGray)),
                    ]);
                }
                spans.extend([
                    Span::styled("d", Style::default().fg(Color::LightRed)),
                    Span::styled(" delete  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                    Span::styled(" close", Style::default().fg(Color::DarkGray)),
                ]);
                spans
            } else {
                vec![
                    Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::LightCyan)),
                    Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                    Span::styled(" preview  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ]
            }
        }
        View::ProjectWizard => {
            vec![
                Span::styled("Tab", Style::default().fg(Color::LightCyan)),
                Span::styled(" switch field  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
                Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::ProjectPicker => {
            vec![
                Span::styled("j/k", Style::default().fg(Color::LightCyan)),
                Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::LightGreen)),
                Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::ProjectDeleteConfirm => {
            vec![
                Span::styled("Enter", Style::default().fg(Color::LightRed)),
                Span::styled(" confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::LightCyan)),
                Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
            ]
        }
        View::RespawnConfirm => vec![],
        View::AgentWizard => vec![
            Span::styled("Tab", Style::default().fg(Color::LightCyan)),
            Span::styled(" next  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Ctrl+S", Style::default().fg(Color::LightGreen)),
            Span::styled(" create  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::LightCyan)),
            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
        ],
    };

    let mut line_spans = help_text;

    let stalled = app.stalled_targets();
    if !stalled.is_empty() {
        let mut banner = vec![Span::styled(
            format!("⚠ {} stalled inbox(es)  ", stalled.len()),
            Style::default().fg(Color::LightRed),
        )];
        banner.append(&mut line_spans);
        line_spans = banner;
    }

    if let Some((msg, _)) = &app.status_message {
        line_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        line_spans.push(Span::styled(msg, Style::default().fg(Color::LightYellow)));
    }

    if matches!(app.view, View::ProjectList | View::TaskList) {
        let tg = telegram_health_spans(app);
        if !tg.is_empty() {
            line_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            line_spans.extend(tg);
        }
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

    let title = format!(
        " Describe task goal (empty = setup only) [{}] (Ctrl+S to continue) ",
        mode.indicator(),
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
            WizardStep::SelectBranch => "Tab: switch mode  j/k: navigate  Enter: next  Esc: back",
            WizardStep::EnterDescription => "Ctrl+S: create task (empty = setup only)  Esc: back",
        };
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    };

    let para = Paragraph::new(content);
    f.render_widget(para, area);
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

#[allow(clippy::too_many_arguments)]
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
            if has_base {
                Constraint::Length(7)
            } else {
                Constraint::Length(3)
            }
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
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Length(1),
                        Constraint::Length(3),
                    ])
                    .split(chunks[1]);

                let branch_focused = !base_branch_focus;
                let branch_border_color = if branch_focused {
                    Color::LightGreen
                } else {
                    Color::DarkGray
                };
                let base_border_color = if base_branch_focus {
                    Color::LightGreen
                } else {
                    Color::DarkGray
                };
                let branch_title_color = if branch_focused {
                    Color::LightGreen
                } else {
                    Color::DarkGray
                };
                let base_title_color = if base_branch_focus {
                    Color::LightGreen
                } else {
                    Color::DarkGray
                };

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
                    branch_editor
                        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
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
                    base_editor
                        .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
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

fn draw_directory_picker(f: &mut Frame, app: &App) {
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let picker = match &app.dir_picker {
        Some(p) => p,
        None => return,
    };

    let title = match picker.origin {
        DirPickerOrigin::NewTask => " Select Repos Directory ",
        DirPickerOrigin::RepoSelect => " Select Repository ",
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
                Some(DirKind::GitRepo) => ("  [git]", Style::default().fg(Color::LightGreen)),
                Some(DirKind::MultiRepoParent) => {
                    ("  [multi]", Style::default().fg(Color::LightYellow))
                }
                _ => ("", Style::default()),
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{}{}/", prefix, name), base_style),
                Span::styled(suffix, suffix_style),
            ])));
        } else {
            items.push(ListItem::new(Span::styled(
                format!("{}{}/", prefix, name),
                base_style,
            )));
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
            let meta_line = Line::from(Span::styled(meta_text, Style::default().fg(meta_color)));

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
        (
            "Review Requests",
            Color::LightCyan,
            &app.show_prs_data.review_requests,
        ),
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
            let mut meta_parts = vec![item.repo_full_name.clone(), item.author.clone()];
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

// ---------------------------------------------------------------------------
// Archive view
// ---------------------------------------------------------------------------

/// Split `text` into segments of (substring, is_match) for case-insensitive
/// highlighting of `query` (already lowercased). Uses char boundaries from the
/// original string so it is safe for non-ASCII content.
fn highlight_segments<'a>(text: &'a str, terms: &[&str]) -> Vec<(&'a str, bool)> {
    if terms.is_empty() {
        return vec![(text, false)];
    }

    let lower = text.to_lowercase();

    // Build a mapping from byte offsets in `lower` back to byte offsets in `text`.
    // Both strings have the same number of chars, but byte widths may differ.
    let text_offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let lower_offsets: Vec<usize> = lower.char_indices().map(|(i, _)| i).collect();

    // Collect all match ranges (as byte offsets in `text`) across all terms
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        let mut search_from = 0;
        while let Some(rel_pos) = lower[search_from..].find(term) {
            let lower_start = search_from + rel_pos;
            let lower_end = lower_start + term.len();

            let char_start = match lower_offsets.binary_search(&lower_start) {
                Ok(i) => i,
                Err(_) => break,
            };
            let char_end = if lower_end == lower.len() {
                text.len()
            } else {
                match lower_offsets.binary_search(&lower_end) {
                    Ok(i) => text_offsets[i],
                    Err(_) => break,
                }
            };
            let text_start = text_offsets[char_start];

            ranges.push((text_start, char_end));
            search_from = lower_end;
        }
    }

    if ranges.is_empty() {
        return vec![(text, false)];
    }

    // Sort by start, then merge overlapping/adjacent ranges
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }

    // Build segments from merged ranges
    let mut segments = Vec::new();
    let mut last = 0;
    for (start, end) in merged {
        if start > last {
            segments.push((&text[last..start], false));
        }
        segments.push((&text[start..end], true));
        last = end;
    }
    if last < text.len() {
        segments.push((&text[last..], false));
    }

    if segments.is_empty() {
        vec![(text, false)]
    } else {
        segments
    }
}

fn format_time_ago(archived_at: &chrono::DateTime<Utc>) -> String {
    let duration = Utc::now().signed_duration_since(*archived_at);
    let days = duration.num_days();
    if days > 0 {
        format!("{}d ago", days)
    } else {
        let hours = duration.num_hours();
        if hours > 0 {
            format!("{}h ago", hours)
        } else {
            let mins = duration.num_minutes();
            format!("{}m ago", mins.max(1))
        }
    }
}

fn draw_archive(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Search input
    let search_title = match app.archive_kind {
        ArchiveKind::Tasks => " Task Archive Search ",
        ArchiveKind::Agents => " AgentRecord Archive Search ",
    };
    let search_block = Block::default()
        .title(search_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightCyan));
    let search_inner = search_block.inner(chunks[0]);
    f.render_widget(search_block, chunks[0]);
    f.render_widget(&app.archive_search, search_inner);

    // Filtered results
    let filtered = app.archive_filtered_indices();
    let query: String = app.archive_search.lines().join("").to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|&idx| {
            match app.archive_kind {
                ArchiveKind::Tasks => {
                    let (task, _) = &app.archive_tasks[idx];
                    let task_name = task.meta.task_id();
                    let time_ago = task
                        .meta
                        .archived_at
                        .as_ref()
                        .map(format_time_ago)
                        .unwrap_or_default();

                    let mut spans: Vec<Span> = Vec::new();

                    // Task name with match highlighting (Unicode-safe)
                    for (seg, is_match) in highlight_segments(&task_name, &terms) {
                        let style = if is_match {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        spans.push(Span::styled(seg.to_string(), style));
                    }

                    // Time ago
                    spans.push(Span::styled(
                        format!("  {}", time_ago),
                        Style::default().fg(Color::DarkGray),
                    ));

                    // Saved badge
                    if task.meta.saved {
                        spans.push(Span::styled(
                            "  [SAVED]",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }

                    ListItem::new(Line::from(spans))
                }
                ArchiveKind::Agents => {
                    let (agent, _) = &app.archive_agents[idx];
                    let agent_name = agent.meta.name.clone();
                    let kind = agent_kind_label(&agent.meta.kind);
                    let time_ago = format_time_ago(&agent.meta.updated_at);

                    let mut spans: Vec<Span> = Vec::new();
                    for (seg, is_match) in highlight_segments(&agent_name, &terms) {
                        let style = if is_match {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        spans.push(Span::styled(seg.to_string(), style));
                    }
                    spans.push(Span::styled(
                        format!("  {kind}"),
                        Style::default().fg(Color::LightMagenta),
                    ));
                    spans.push(Span::styled(
                        format!("  {}", agent.meta.project),
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.push(Span::styled(
                        format!("  {}", time_ago),
                        Style::default().fg(Color::DarkGray),
                    ));

                    ListItem::new(Line::from(spans))
                }
            }
        })
        .collect();

    let title = match app.archive_kind {
        ArchiveKind::Tasks => format!(" Task Archive ({}) ", filtered.len()),
        ArchiveKind::Agents => format!(" AgentRecord Archive ({}) ", filtered.len()),
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title_bottom(clock_title(app)),
        )
        .highlight_style(Style::default().bg(Color::Rgb(40, 40, 50)));

    // Clamp selection
    if !filtered.is_empty() && app.archive_selected >= filtered.len() {
        app.archive_selected = filtered.len() - 1;
    }
    app.archive_list_state.select(if filtered.is_empty() {
        None
    } else {
        Some(app.archive_selected)
    });

    f.render_stateful_widget(list, chunks[1], &mut app.archive_list_state);
}

fn draw_archive_preview(f: &mut Frame, app: &mut App) {
    let content = match &app.archive_preview {
        Some(c) => c.clone(),
        None => return,
    };

    let filtered = app.archive_filtered_indices();
    let title = match app.archive_kind {
        ArchiveKind::Tasks => {
            let task_name = filtered
                .get(app.archive_selected)
                .and_then(|&i| app.archive_tasks.get(i))
                .map(|(t, _)| t.meta.task_id())
                .unwrap_or_default();
            let saved = filtered
                .get(app.archive_selected)
                .and_then(|&i| app.archive_tasks.get(i))
                .map(|(t, _)| t.meta.saved)
                .unwrap_or(false);
            if saved {
                format!(" {} [SAVED] ", task_name)
            } else {
                format!(" {} ", task_name)
            }
        }
        ArchiveKind::Agents => filtered
            .get(app.archive_selected)
            .and_then(|&i| app.archive_agents.get(i))
            .map(|(agent, _)| {
                format!(
                    " {} {}--{} ",
                    agent_kind_label(&agent.meta.kind),
                    agent.meta.project,
                    agent.meta.name
                )
            })
            .unwrap_or_else(|| " AgentRecord Archive ".to_string()),
    };

    let area = centered_rect(80, 80, f.area());
    f.render_widget(Clear, area);

    let query: String = app.archive_search.lines().join("").to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    let lines: Vec<Line> = content
        .lines()
        .map(|line| {
            let segments = highlight_segments(line, &terms);
            let spans: Vec<Span> = segments
                .into_iter()
                .map(|(seg, is_match)| {
                    let style = if is_match {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    Span::styled(seg.to_string(), style)
                })
                .collect();
            Line::from(spans)
        })
        .collect();

    let total_lines = lines.len() as u16;
    let inner_height = area.height.saturating_sub(2); // borders
    let max_scroll = total_lines.saturating_sub(inner_height);
    if app.archive_scroll > max_scroll {
        app.archive_scroll = max_scroll;
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightCyan)),
        )
        .scroll((app.archive_scroll, 0))
        .wrap(Wrap { trim: false });

    f.render_widget(paragraph, area);
}

fn draw_settings(f: &mut Frame, app: &mut App, area: Rect) {
    let retention_days = app.archive_retention_days;
    let harness_kind = app.config.harness_kind();

    let selected_style = Style::default().bg(Color::Rgb(40, 40, 50));

    let retention_display = format!(
        "  Archive retention   \u{25C0}  {} days \u{25B6}",
        retention_days
    );
    let harness_display = format!(
        "  Harness             \u{25C0}  {:<6} \u{25B6}",
        harness_kind.as_str()
    );

    // Telegram token display: mask all but last 4 chars
    let token_text: String = app.telegram_token_editor.lines().join("");
    let token_display = if token_text.is_empty() {
        "(not set)".to_string()
    } else if token_text.len() <= 4 {
        token_text.clone()
    } else {
        format!(
            "{}{}",
            "\u{2022}".repeat(token_text.len() - 4),
            &token_text[token_text.len() - 4..]
        )
    };

    let chat_id_text: String = app.telegram_chat_id_editor.lines().join("");
    let chat_id_display = if chat_id_text.is_empty() {
        "(not set)".to_string()
    } else {
        chat_id_text.clone()
    };

    // If editing a telegram field, we render the TextArea separately. Token
    // is now at row 2 (after retention/harness); chat-id at 3.
    let editing_token = app.settings_editing && app.settings_selected == 2;
    let editing_chat_id = app.settings_editing && app.settings_selected == 3;

    let token_row_text = format!(
        "  Telegram bot token  {}",
        if editing_token { "" } else { &token_display }
    );
    let chat_id_row_text = format!(
        "  Telegram chat ID    {}",
        if editing_chat_id {
            ""
        } else {
            &chat_id_display
        }
    );

    let items = vec![
        ListItem::new(Line::from(vec![
            Span::styled(&retention_display, Style::default().fg(Color::White)),
            Span::styled(
                "    (h/l to adjust, 7\u{2013}365 days)",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .style(if app.settings_selected == 0 {
            selected_style
        } else {
            Style::default()
        }),
        ListItem::new(Line::from(vec![
            Span::styled(&harness_display, Style::default().fg(Color::White)),
            Span::styled(
                "    (h/l to switch \u{2014} applies to newly-spawned agents only)",
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .style(if app.settings_selected == 1 {
            selected_style
        } else {
            Style::default()
        }),
        ListItem::new(Line::from(vec![
            Span::styled(&token_row_text, Style::default().fg(Color::White)),
            if !editing_token {
                Span::styled("    (Enter to edit)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]))
        .style(if app.settings_selected == 2 {
            selected_style
        } else {
            Style::default()
        }),
        ListItem::new(Line::from(vec![
            Span::styled(&chat_id_row_text, Style::default().fg(Color::White)),
            if !editing_chat_id {
                Span::styled("    (Enter to edit)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]))
        .style(if app.settings_selected == 3 {
            selected_style
        } else {
            Style::default()
        }),
    ];

    let block = Block::default()
        .title(" Settings ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title_bottom(clock_title(app));

    let inner_area = block.inner(area);
    f.render_widget(block, area);

    // Render the list items (each is 1 row high)
    let list = List::new(items);
    f.render_widget(list, inner_area);

    // Overlay TextArea widget when editing a telegram field
    if editing_token || editing_chat_id {
        let row_index = if editing_token { 2 } else { 3 };
        // The label prefix is 22 chars ("  Telegram bot token  " / "  Telegram chat ID    ")
        let label_width = 22u16;
        if inner_area.height > row_index && inner_area.width > label_width {
            let editor_area = Rect {
                x: inner_area.x + label_width,
                y: inner_area.y + row_index,
                width: inner_area.width.saturating_sub(label_width),
                height: 1,
            };
            let editor = if editing_token {
                &mut app.telegram_token_editor
            } else {
                &mut app.telegram_chat_id_editor
            };
            editor.set_cursor_line_style(Style::default().bg(Color::Rgb(40, 40, 50)));
            f.render_widget(&*editor, editor_area);
        }
    }
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
    let border_color = if is_focused {
        Color::LightCyan
    } else {
        Color::DarkGray
    };

    // Build title from relative path
    let title = if nv.current_dir == nv.root_dir {
        " Notes ".to_string()
    } else {
        let rel = nv
            .current_dir
            .strip_prefix(&nv.root_dir)
            .unwrap_or(&nv.current_dir);
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
        let entry_name = nv
            .entries
            .get(nv.selected_index)
            .map(|e| e.name.as_str())
            .unwrap_or("?");
        let items: Vec<ListItem> = nv
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
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
            })
            .collect();

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
            let confirm = Paragraph::new(msg).style(
                Style::default()
                    .fg(Color::LightRed)
                    .bg(Color::Rgb(40, 20, 20)),
            );
            f.render_widget(confirm, confirm_area);
        }
        return;
    }

    // If create_input is active, show it at the bottom
    if let Some((ref input, is_dir)) = nv.create_input {
        let label = if is_dir { "New dir: " } else { "New note: " };

        let items: Vec<ListItem> = nv
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
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
            })
            .collect();

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
            let input_para = Paragraph::new(text).style(Style::default().fg(Color::LightGreen));
            f.render_widget(input_para, input_area);
        }
        return;
    }

    // If rename_input is active, show it inline at the selected position
    if let Some(ref _rename) = nv.rename_input {
        let items: Vec<ListItem> = nv
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                if i == nv.selected_index {
                    let rename_text = nv.rename_input.as_ref().unwrap().lines()[0].clone();
                    let display = format!("  {}", rename_text);
                    ListItem::new(display).style(
                        Style::default()
                            .fg(Color::LightYellow)
                            .bg(Color::Rgb(40, 40, 50)),
                    )
                } else {
                    let display = if entry.is_dir {
                        format!("  {}/", entry.name)
                    } else {
                        format!("  {}", entry.name)
                    };
                    ListItem::new(display).style(Style::default())
                }
            })
            .collect();

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

    let items: Vec<ListItem> = nv
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_cut = nv
                .cut_entry
                .as_ref()
                .is_some_and(|(dir, name)| dir == &nv.current_dir && name == &entry.file_name);
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
        })
        .collect();

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
        let file_name = path
            .file_stem()
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
        nv.editor
            .textarea
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
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

#[cfg(test)]
mod project_count_cell_tests {
    use super::*;

    fn span_text(spans: &[Span<'_>]) -> Vec<String> {
        spans.iter().map(|span| span.content.to_string()).collect()
    }

    #[test]
    fn count_cell_right_aligns_and_colors_active_count() {
        let mut spans = Vec::new();

        push_project_count_cell(&mut spans, 3, 12, 8);

        assert_eq!(span_text(&spans), vec!["    ", "3", "/12"]);
        assert_eq!(spans[1].style, Style::default().fg(Color::LightGreen));
        assert_eq!(spans[2].style, dim_count_style());
    }

    #[test]
    fn count_cell_dims_zero_active_count() {
        let mut spans = Vec::new();

        push_project_count_cell(&mut spans, 0, 8, 8);

        assert_eq!(span_text(&spans), vec!["     ", "0", "/8"]);
        assert_eq!(spans[1].style, dim_count_style());
        assert_eq!(spans[2].style, dim_count_style());
    }

    #[test]
    fn truncation_keeps_utf8_boundaries() {
        assert_eq!(
            truncate_with_ellipsis("drua workstream — local repo", 18),
            "drua workstream —…"
        );
    }
}

#[cfg(test)]
mod agent_status_tests {
    use super::*;

    fn sample(now: Instant, age: Duration, command: &str) -> AgentActivitySample {
        AgentActivitySample {
            last_tmux_activity_epoch: Some(1),
            last_observed_work_at: now.checked_sub(age),
            foreground_command: command.to_string(),
            pane_dead: false,
            query_ok: true,
        }
    }

    #[test]
    fn recent_non_shell_activity_is_working() {
        let now = Instant::now();
        let sample = sample(now, Duration::from_secs(2), "codex-aarch64-a");

        assert_eq!(
            classify_agent_status(now, Some(&sample)),
            WorkingIdle::Working
        );
    }

    #[test]
    fn old_activity_is_idle() {
        let now = Instant::now();
        let sample = sample(now, Duration::from_secs(11), "codex-aarch64-a");

        assert_eq!(classify_agent_status(now, Some(&sample)), WorkingIdle::Idle);
    }

    #[test]
    fn shell_or_failed_query_is_idle() {
        let now = Instant::now();
        let shell = sample(now, Duration::from_secs(1), "zsh");
        let mut failed = sample(now, Duration::from_secs(1), "codex-aarch64-a");
        failed.query_ok = false;

        assert_eq!(classify_agent_status(now, Some(&shell)), WorkingIdle::Idle);
        assert_eq!(classify_agent_status(now, Some(&failed)), WorkingIdle::Idle);
        assert_eq!(classify_agent_status(now, None), WorkingIdle::Idle);
    }
}
