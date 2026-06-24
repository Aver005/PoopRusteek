use crate::app::AppState;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

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
    let visible_prompt = if input.is_empty() {
        "Describe the change, bug, or command you want."
    } else {
        &viewport.text
    };

    let mut first_line = vec![Span::styled(
        prefix,
        Style::default().fg(theme.accent).bg(theme.input_bg),
    )];
    first_line.extend(cursor_spans(
        visible_prompt,
        cursor_in_view.min(visible_prompt.chars().count()),
        !state.is_generating,
        theme,
        input.is_empty(),
    ));

    let paragraph = if input.is_empty() {
        Paragraph::new(vec![
            Line::from(first_line),
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
            Line::from(first_line),
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
}

struct VisibleWindow {
    text: String,
    start_char: usize,
}

fn cursor_spans(
    text: &str,
    cursor_char: usize,
    show_cursor: bool,
    theme: &Theme,
    is_placeholder: bool,
) -> Vec<Span<'static>> {
    if !show_cursor {
        return vec![Span::styled(
            text.to_string(),
            Style::default()
                .fg(if is_placeholder { theme.text_dim } else { theme.fg })
                .bg(theme.input_bg),
        )];
    }

    let chars: Vec<char> = text.chars().collect();
    let clamped = cursor_char.min(chars.len());
    let before: String = chars.iter().take(clamped).collect();
    let current = chars.get(clamped).copied().unwrap_or(' ');
    let after: String = chars.iter().skip(clamped + usize::from(clamped < chars.len())).collect();

    let mut spans = Vec::new();
    if !before.is_empty() {
        spans.push(Span::styled(
            before,
            Style::default()
                .fg(if is_placeholder { theme.text_dim } else { theme.fg })
                .bg(theme.input_bg),
        ));
    }

    spans.push(Span::styled(
        current.to_string(),
        Style::default()
            .fg(theme.bg)
            .bg(theme.accent_soft)
            .add_modifier(Modifier::BOLD),
    ));

    if !after.is_empty() {
        spans.push(Span::styled(
            after,
            Style::default()
                .fg(if is_placeholder { theme.text_dim } else { theme.fg })
                .bg(theme.input_bg),
        ));
    }

    spans
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
