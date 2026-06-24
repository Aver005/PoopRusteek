use crate::app::AppState;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub fn render_input(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let border_style = theme.border_style(!state.is_generating);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(if state.is_generating {
            " Generating... "
        } else {
            " Message "
        })
        .title_style(theme.bold_accent_style())
        .style(Style::default().bg(theme.input_bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let input = &state.input_buffer;
    let cursor = state.input_cursor;

    let mut spans: Vec<Span> = Vec::new();

    if input.is_empty() {
        spans.push(Span::styled(
            "Type a message...",
            Style::default().fg(theme.text_dim),
        ));
    } else {
        let before_cursor = &input[..cursor];
        let at_cursor = if cursor < input.len() {
            &input[cursor..cursor + 1]
        } else {
            ""
        };
        let after_cursor = if cursor < input.len() {
            &input[cursor + 1..]
        } else {
            ""
        };

        spans.push(Span::styled(before_cursor, Style::default().fg(theme.fg)));
        if at_cursor.is_empty() {
            spans.push(Span::styled(
                " ",
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                at_cursor,
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(after_cursor, Style::default().fg(theme.fg)));
        }
    }

    let paragraph = Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.input_bg));
    frame.render_widget(paragraph, inner);
}
