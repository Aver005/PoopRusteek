use crate::app::AppState;
use crate::provider::Role;
use crate::tui::markdown::render_markdown;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub fn render_chat(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(false))
        .title(" Conversation ")
        .title_style(Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.messages.is_empty() && !state.is_generating {
        let welcome = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Pooprusteek",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " v0.1.0",
                    Style::default().fg(theme.text_dim),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  A fast TUI coding agent powered by DeepSeek",
                    Style::default().fg(theme.text_dim),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Type your message and press Enter to start.",
                    Style::default().fg(theme.text_dim),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Ctrl+C quit  |  Ctrl+L clear",
                    Style::default().fg(theme.text_dim),
                ),
            ]),
        ];

        let paragraph = Paragraph::new(welcome)
            .style(Style::default().bg(theme.bg))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, inner);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    for msg in &state.messages {
        match msg.role {
            Role::User => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " prompt ",
                        Style::default()
                            .fg(theme.bg)
                            .bg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                for line in msg.visible_content().lines() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {line}"),
                            Style::default().fg(theme.fg).bg(theme.user_bg),
                        ),
                    ]));
                }
                lines.push(Line::from(""));
            }
            Role::Assistant => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " pooprusteek ",
                        Style::default()
                            .fg(theme.bg)
                            .bg(theme.success)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                if msg.content.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  Thinking...",
                            Style::default().fg(theme.text_dim),
                        ),
                    ]));
                } else {
                    let mut md_lines = render_markdown(msg.visible_content(), theme);
                    for line in &mut md_lines {
                        let mut styled_spans: Vec<Span> = Vec::new();
                        styled_spans.push(Span::styled(
                            "  ",
                            Style::default().fg(theme.fg).bg(theme.assistant_bg),
                        ));
                        for span in line.spans.iter() {
                            styled_spans.push(span.clone());
                        }
                        *line = Line::from(styled_spans);
                    }
                    lines.extend(md_lines);
                }
                lines.push(Line::from(""));
            }
            Role::System => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " system ",
                        Style::default().fg(theme.bg).bg(theme.warning),
                    ),
                    Span::styled(
                        format!(" {}", msg.visible_content()),
                        Style::default().fg(theme.text_dim),
                    ),
                ]));
            }
            Role::Tool => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" tool · {} ", msg.name.as_deref().unwrap_or("unknown")),
                        Style::default().fg(theme.bg).bg(theme.accent_soft),
                    ),
                ]));
                for line in msg.visible_content().lines() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {line}"),
                            Style::default().fg(theme.text_soft).bg(theme.tool_bg),
                        ),
                    ]));
                }
                lines.push(Line::from(""));
            }
        }
    }

    let total_lines = lines.len();
    let visible_height = inner.height as usize;
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = (state.scroll_offset as usize).min(max_scroll);
    let start = scroll;
    let visible: Vec<Line> = lines.into_iter().skip(start).take(visible_height).collect();

    let paragraph = Paragraph::new(visible)
        .style(Style::default().bg(theme.panel))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}
