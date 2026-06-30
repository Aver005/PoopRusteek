use crate::app::AppState;
use crate::provider::{estimate_tokens, Role};
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

fn format_time(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        dt.format("%H:%M:%S").to_string()
    } else {
        rfc3339.chars().take(8).collect()
    }
}

fn meta_line(msg: &crate::provider::ChatMessage, theme: &Theme) -> Line<'static> {
    let time = format_time(&msg.created_at);
    let tokens = msg.total_tokens.unwrap_or_else(|| estimate_tokens(&msg.content));
    let mut parts = vec![format!(" {}  {}t ", time, tokens)];
    if msg.think_elapsed_secs > 0.0 {
        parts.push(format!("think {:.1}s", msg.think_elapsed_secs));
    }
    if msg.references_count > 0 {
        parts.push(format!("\u{1F4CE}{}", msg.references_count));
    }
    let meta = parts.join("  ");
    Line::from(vec![Span::styled(
        meta,
        Style::default().fg(theme.text_dim).bg(theme.bg),
    )])
}

pub fn render_chat(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &state.focused().messages {
        match msg.role {
            Role::User => {
                for line_text in msg.visible_content().lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!(" {} ", line_text),
                        Style::default().fg(theme.fg).bg(theme.user_bg),
                    )]));
                }
                lines.push(meta_line(msg, theme));
            }
            Role::Assistant => {
                let header_color = match msg.status.as_deref() {
                    Some("ABORTED") => theme.error,
                    Some("WIP") => theme.warning,
                    _ => theme.success,
                };
                let model_tag = if !msg.model.is_empty() {
                    format!(" ({})", msg.model)
                } else {
                    String::new()
                };
                let header = Span::styled(
                    format!(" pooprusteek{} ", model_tag),
                    Style::default()
                        .fg(theme.bg)
                        .bg(header_color)
                        .add_modifier(Modifier::BOLD),
                );
                if msg.content.is_empty() {
                    lines.push(Line::from(vec![
                        header,
                        Span::styled(
                            " Thinking...",
                            Style::default().fg(theme.text_dim),
                        ),
                    ]));
                } else {
                    let mut md_lines = crate::tui::markdown::render_markdown(msg.visible_content(), theme);
                    for md_line in &mut md_lines {
                        let mut styled: Vec<Span> = Vec::new();
                        for span in md_line.spans.iter() {
                            styled.push(span.clone());
                        }
                        *md_line = Line::from(styled);
                    }
                    compact_lines(&mut md_lines);
                    lines.push(Line::from(vec![header]));
                    lines.extend(md_lines);
                    lines.push(meta_line(msg, theme));
                }
            }
            Role::System => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " \u{2139} ",
                        Style::default().fg(theme.warning).bg(theme.bg),
                    ),
                    Span::styled(
                        msg.visible_content(),
                        Style::default().fg(theme.text_dim).bg(theme.bg),
                    ),
                ]));
            }
            Role::Tool => {
                let tool_name = msg.name.as_deref().unwrap_or("unknown");
                let icon = if msg.tool_error { "\u{2717}" } else { "\u{2713}" };
                let status_color = if msg.tool_error { theme.error } else { theme.success };

                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {icon} "),
                        Style::default()
                            .fg(theme.bg)
                            .bg(status_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {tool_name} "),
                        Style::default()
                            .fg(theme.bg)
                            .bg(if msg.tool_error { theme.error } else { theme.accent })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if msg.tool_error { " failed " } else { " done " },
                        Style::default().fg(theme.text_dim).bg(theme.tool_bg),
                    ),
                ]));

                let body = msg.visible_content();
                let body_lines: Vec<&str> = body.lines().collect();
                if body_lines.iter().all(|l| l.trim().is_empty()) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  (no output)",
                            Style::default().fg(theme.text_dim).bg(theme.tool_bg),
                        ),
                    ]));
                } else {
                    for line_text in &body_lines {
                        lines.push(Line::from(vec![
                            Span::styled(
                                " \u{2502} ",
                                Style::default().fg(theme.accent_soft).bg(theme.tool_bg),
                            ),
                            Span::styled(
                                (*line_text).to_string(),
                                Style::default()
                                    .fg(if msg.tool_error { theme.error } else { theme.text_soft })
                                    .bg(theme.tool_bg),
                            ),
                        ]));
                    }
                }
            }
        }
        lines.push(Line::from(""));
    }

    let total_rows = count_wrapped_rows(&lines, area.width as usize);
    let visible_height = area.height as usize;
    let max_scroll = total_rows.saturating_sub(visible_height);
    let scroll_from_bottom = (state.scroll_offset as usize).min(max_scroll);
    let top_row = max_scroll.saturating_sub(scroll_from_bottom);

    let paragraph = Paragraph::new(lines)
        .style(Style::default().bg(theme.bg))
        .wrap(Wrap { trim: false })
        .scroll((top_row as u16, 0));
    frame.render_widget(paragraph, area);
}

fn is_blank(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn compact_lines(lines: &mut Vec<Line<'static>>) {
    // Remove leading blank lines
    while lines.first().is_some_and(|l| is_blank(l)) {
        lines.remove(0);
    }
}

fn count_wrapped_rows(lines: &[Line], width: usize) -> usize {
    if width == 0 {
        return lines.len().max(1);
    }
    let mut total = 0usize;
    for line in lines {
        let text: String = line.spans.iter().map(|s| &*s.content).collect();
        if text.is_empty() {
            total += 1;
        } else {
            let char_count = text.chars().count();
            total += char_count.div_ceil(width).max(1);
        }
    }
    total.max(1)
}
