use crate::app::AppState;
use crate::provider::Role;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

pub fn render_chat(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &state.messages {
        match msg.role {
            Role::User => {
                for line_text in msg.visible_content().lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!(" {} ", line_text),
                        Style::default().fg(theme.fg).bg(theme.user_bg),
                    )]));
                }
            }
            Role::Assistant => {
                let header = Span::styled(
                    " pooprusteek ",
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.success)
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
