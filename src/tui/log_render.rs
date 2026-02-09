use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLineKind {
    AgentStart,
    AgentFinish,
    UserFeedbackStart,
    UserFeedbackEnd,
    UserFeedbackBody,
    StopConditionSuccess,
    StopConditionFailure,
    StopConditionInput,
    ErrorLine,
    TrimmedIndicator,
    Normal,
}

fn classify_line(line: &str, in_feedback_block: &mut bool) -> LogLineKind {
    let trimmed = line.trim();

    // Feedback block markers
    if trimmed.starts_with("--- User feedback at ") && trimmed.ends_with("---") {
        *in_feedback_block = true;
        return LogLineKind::UserFeedbackStart;
    }
    if trimmed == "--- End user feedback ---" {
        *in_feedback_block = false;
        return LogLineKind::UserFeedbackEnd;
    }

    // Inside a feedback block
    if *in_feedback_block {
        return LogLineKind::UserFeedbackBody;
    }

    // Agent lifecycle markers
    if trimmed.starts_with("--- Agent:") && trimmed.ends_with("---") {
        if trimmed.contains("started at") {
            return LogLineKind::AgentStart;
        }
        if trimmed.contains("finished at") {
            return LogLineKind::AgentFinish;
        }
    }

    // Trimmed indicator
    if trimmed.starts_with("[...") && trimmed.ends_with("trimmed ...]") {
        return LogLineKind::TrimmedIndicator;
    }

    // Stop condition magic strings
    if trimmed.contains("AGENT_DONE")
        || trimmed.contains("TASK_COMPLETE")
        || trimmed.contains("TESTS_PASS")
    {
        return LogLineKind::StopConditionSuccess;
    }
    if trimmed.contains("TASK_BLOCKED") || trimmed.contains("TESTS_FAIL") {
        return LogLineKind::StopConditionFailure;
    }
    if trimmed.contains("INPUT_NEEDED") {
        return LogLineKind::StopConditionInput;
    }

    // Error indicators
    let lower = line.to_lowercase();
    if lower.contains("error") || lower.contains("failed") || lower.contains("[stderr]") {
        return LogLineKind::ErrorLine;
    }

    LogLineKind::Normal
}

fn style_structural_line<'a>(line: &'a str, kind: LogLineKind) -> Line<'a> {
    match kind {
        LogLineKind::AgentStart | LogLineKind::AgentFinish => {
            // Try to highlight the agent name separately
            if let Some(name_start) = line.find("Agent: ") {
                let prefix_end = name_start + 7; // "Agent: ".len()
                let rest = &line[prefix_end..];
                let name_end = rest
                    .find(|c: char| c == ' ' || c == '\t')
                    .unwrap_or(rest.len());
                let agent_name = &rest[..name_end];
                let after_name = &rest[name_end..];

                Line::from(vec![
                    Span::styled(
                        &line[..prefix_end],
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        agent_name,
                        Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        after_name,
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(Span::styled(
                    line,
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ))
            }
        }
        LogLineKind::UserFeedbackStart | LogLineKind::UserFeedbackEnd => Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )),
        LogLineKind::UserFeedbackBody => {
            Line::from(Span::styled(line, Style::default().fg(Color::LightYellow)))
        }
        LogLineKind::StopConditionSuccess => Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        )),
        LogLineKind::StopConditionFailure => Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        )),
        LogLineKind::StopConditionInput => Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )),
        LogLineKind::ErrorLine => {
            Line::from(Span::styled(line, Style::default().fg(Color::LightRed)))
        }
        LogLineKind::TrimmedIndicator => Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
        LogLineKind::Normal => Line::from(Span::styled(line, Style::default().fg(Color::Gray))),
    }
}

fn render_markdown_lines(text: &str) -> Vec<Line<'static>> {
    let parser = Parser::new_ext(text, Options::empty());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    // Style state
    let mut bold = false;
    let mut italic = false;
    let mut in_code_block = false;
    let mut in_heading: Option<u8> = None; // heading level
    let mut list_depth: usize = 0;
    let mut ordered_index: Option<u64> = None;

    let code_style = Style::default().fg(Color::Rgb(180, 180, 200));

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // Flush current line
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_heading = Some(level as u8);
            }
            Event::End(TagEnd::Heading(_)) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_heading = None;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_code_block = false;
            }
            Event::Start(Tag::Emphasis) => {
                italic = true;
            }
            Event::End(TagEnd::Emphasis) => {
                italic = false;
            }
            Event::Start(Tag::Strong) => {
                bold = true;
            }
            Event::End(TagEnd::Strong) => {
                bold = false;
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(format!("`{}`", code), code_style));
            }
            Event::Start(Tag::List(start)) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                list_depth += 1;
                ordered_index = start;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                if list_depth == 0 {
                    ordered_index = None;
                }
            }
            Event::Start(Tag::Item) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                let bullet = if let Some(ref mut idx) = ordered_index {
                    let s = format!("{}{}. ", indent, idx);
                    *idx += 1;
                    s
                } else {
                    format!("{}\u{2022} ", indent) // bullet: •
                };
                current_spans.push(Span::styled(bullet, Style::default().fg(Color::DarkGray)));
            }
            Event::End(TagEnd::Item) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::Start(Tag::Paragraph) => {
                // Start a new line for paragraphs (unless already at line start)
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::Text(text) => {
                let owned = text.to_string();

                if in_code_block {
                    // Code block: render each line separately
                    for code_line in owned.split('\n') {
                        if !current_spans.is_empty() {
                            lines.push(Line::from(std::mem::take(&mut current_spans)));
                        }
                        current_spans.push(Span::styled(
                            format!("  {}", code_line),
                            code_style,
                        ));
                    }
                } else if let Some(level) = in_heading {
                    let style = match level {
                        1 => Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                        2 => Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                        _ => Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::BOLD),
                    };
                    let prefix = "#".repeat(level as usize);
                    current_spans
                        .push(Span::styled(format!("{} {}", prefix, owned), style));
                } else {
                    let mut style = Style::default().fg(Color::Gray);
                    if bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    current_spans.push(Span::styled(owned, style));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::Rule => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                lines.push(Line::from(Span::styled(
                    "\u{2500}".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }

    // Flush remaining spans
    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    // If markdown parsing produced nothing useful, fall back to plain gray lines
    if lines.is_empty() && !text.is_empty() {
        for line in text.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }

    lines
}

/// Render raw agent.log text into styled ratatui Lines.
pub fn render_log_lines(raw_text: &str) -> Vec<Line<'static>> {
    let mut result: Vec<Line<'static>> = Vec::new();
    let mut in_feedback_block = false;
    let mut normal_batch: Vec<String> = Vec::new();

    let flush_normal = |batch: &mut Vec<String>, result: &mut Vec<Line<'static>>| {
        if batch.is_empty() {
            return;
        }
        let text = batch.join("\n");
        let rendered = render_markdown_lines(&text);
        result.extend(rendered);
        batch.clear();
    };

    for line in raw_text.lines() {
        let kind = classify_line(line, &mut in_feedback_block);

        if kind == LogLineKind::Normal {
            normal_batch.push(line.to_string());
        } else {
            // Flush any pending normal lines through markdown renderer
            flush_normal(&mut normal_batch, &mut result);
            result.push(style_structural_line(line, kind).into_owned());
        }
    }

    // Flush remaining normal lines
    flush_normal(&mut normal_batch, &mut result);

    result
}

/// Extension trait to convert Line<'a> into Line<'static>
trait IntoOwned {
    fn into_owned(self) -> Line<'static>;
}

impl<'a> IntoOwned for Line<'a> {
    fn into_owned(self) -> Line<'static> {
        Line::from(
            self.spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect::<Vec<_>>(),
        )
    }
}
