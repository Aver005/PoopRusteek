use crate::app::AppState;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

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
    let cursor = state.input_cursor.min(input.chars().count());

    let paragraph = if input.is_empty() {
        Paragraph::new(Line::styled(
            "Type a message...",
            Style::default().fg(theme.text_dim),
        ))
        .style(Style::default().bg(theme.input_bg))
    } else {
        Paragraph::new(Line::styled(
            input.as_str(),
            Style::default().fg(theme.fg),
        ))
        .style(Style::default().bg(theme.input_bg))
    };
    frame.render_widget(paragraph, inner);

    let before_cursor: String = input.chars().take(cursor).collect();
    let cursor_offset = u16::try_from(UnicodeWidthStr::width(before_cursor.as_str()))
        .unwrap_or(u16::MAX)
        .min(inner.width.saturating_sub(1));
    frame.set_cursor_position((inner.x + cursor_offset, inner.y));
}
