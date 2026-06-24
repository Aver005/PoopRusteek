use crate::app::AppState;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

pub fn render_input(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    is_landing: bool,
) {
    let border_style = theme.border_style(true);
    let title = if state.is_generating {
        " Compose · generating "
    } else if is_landing {
        " Ask Pooprusteek "
    } else {
        " Compose "
    };
    let hint = if state.is_generating {
        " model is working "
    } else {
        " enter to send "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title)
        .title_style(
            Style::default()
                .fg(theme.accent_soft)
                .bg(theme.input_bg)
                .add_modifier(Modifier::BOLD),
        )
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::default().fg(theme.text_dim).bg(theme.input_bg),
        )))
        .style(Style::default().bg(theme.input_bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let input = &state.input_buffer;
    let cursor = state.input_cursor.min(input.chars().count());
    let prefix = if is_landing { " prompt > " } else { " ask > " };
    let available_width = inner.width.saturating_sub(prefix.len() as u16).max(1);
    let viewport = visible_window(input, cursor, available_width as usize);
    let cursor_in_view = cursor.saturating_sub(viewport.start_char);

    let paragraph = if input.is_empty() {
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(theme.accent).bg(theme.input_bg)),
                Span::styled(
                    "Describe the change, bug, or command you want.",
                    Style::default().fg(theme.text_dim).bg(theme.input_bg),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "       tools, files and command output are stitched into the next turn automatically",
                    Style::default().fg(theme.text_soft).bg(theme.input_bg),
                ),
            ]),
        ])
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(theme.input_bg))
    } else {
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(theme.accent).bg(theme.input_bg)),
                Span::styled(
                    viewport.text.clone(),
                    Style::default().fg(theme.fg).bg(theme.input_bg),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!(
                        "       {} chars  |  cursor {}",
                        input.chars().count(),
                        cursor + 1
                    ),
                    Style::default().fg(theme.text_dim).bg(theme.input_bg),
                ),
            ]),
        ])
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(theme.input_bg))
    };
    frame.render_widget(paragraph, inner);

    let before_cursor: String = viewport.text.chars().take(cursor_in_view).collect();
    let cursor_offset = u16::try_from(UnicodeWidthStr::width(before_cursor.as_str()))
        .unwrap_or(u16::MAX)
        .min(available_width.saturating_sub(1));
    frame.set_cursor_position((inner.x + prefix.len() as u16 + cursor_offset, inner.y));
}

struct VisibleWindow {
    text: String,
    start_char: usize,
}

fn visible_window(text: &str, cursor: usize, max_width: usize) -> VisibleWindow {
    let total_chars = text.chars().count();
    if total_chars <= max_width {
        return VisibleWindow {
            text: text.to_string(),
            start_char: 0,
        };
    }

    let mut start_char = cursor.saturating_sub(max_width.saturating_sub(6));
    if start_char + max_width > total_chars {
        start_char = total_chars.saturating_sub(max_width);
    }

    let visible_text: String = text.chars().skip(start_char).take(max_width).collect();
    VisibleWindow {
        text: visible_text,
        start_char,
    }
}
