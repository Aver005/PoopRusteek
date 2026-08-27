//! Shared building blocks for centered popups/modals. Every modal in the
//! app follows the same skeleton — a centered `Rect`, a bordered `Block`
//! with a bold colored title on the panel background, a run of content
//! lines padded to the popup height, then a separator + hint footer.
//! These helpers keep that skeleton in one place; each modal supplies only
//! its content lines and accent color.

use crate::tui::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

/// Center a `width` x `height` popup inside `area`.
pub(super) fn center_popup(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// The standard modal frame: all borders, `accent`-colored border and bold
/// title, panel background.
pub(super) fn modal_block(
    title: impl Into<Line<'static>>,
    accent: Color,
    theme: &Theme,
) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(title)
        .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel))
}

/// The two-space-indented horizontal rule used above every modal footer.
pub(super) fn separator_line(inner_w: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {}", "\u{2500}".repeat(inner_w.saturating_sub(4))),
        Style::default().fg(theme.border).bg(theme.panel),
    ))
}

/// Строка «список обрезан» над/под рядами. Пикер, удаление сессий и модалка
/// вопроса рисовали её тремя одинаковыми копиями.
pub(super) fn overflow_line(up: bool, theme: &Theme) -> Line<'static> {
    let text = if up {
        "  \u{2191} more "
    } else {
        "  \u{2193} more "
    };
    Line::from(Span::styled(
        text,
        Style::default().fg(theme.text_dim).bg(theme.panel),
    ))
}

/// Маркер курсора в списках вариантов. Один на все мастера: знак был записан
/// тремя способами, а в автодополнении оказался вообще другим (U+25B8).
pub(super) const CURSOR_MARK: &str = "\u{25B6} ";

/// Строка варианта в мастере: маркер курсора плюс подпись, без фона.
/// Четыре мастера рисовали её порознь и уже разошлись в стиле отбивки.
pub(super) fn option_row(
    label: impl Into<String>,
    is_cursor: bool,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.fg)),
        Span::styled(
            if is_cursor { CURSOR_MARK } else { "  " },
            Style::default().fg(if is_cursor {
                theme.accent
            } else {
                theme.text_dim
            }),
        ),
        Span::styled(
            label.into(),
            if is_cursor {
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_soft)
            },
        ),
    ])
}

/// Отрисовать собранную модалку: затереть площадку, рамку, содержимое.
/// Одинаковый хвост стоял в семи отрисовщиках.
pub(super) fn draw_popup(
    frame: &mut ratatui::Frame,
    popup_area: Rect,
    inner: Rect,
    block: Block<'static>,
    lines: Vec<Line<'static>>,
    theme: &Theme,
) {
    frame.render_widget(ratatui::widgets::Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}

/// Pad `lines` with blanks until it is `target_rows` long, so the footer
/// lands on the popup's bottom rows. The popup paragraph's own panel-bg
/// style paints the padding.
pub(super) fn fill_panel_space(lines: &mut Vec<Line<'static>>, target_rows: usize) {
    while lines.len() < target_rows {
        lines.push(Line::from(""));
    }
}

/// Render one row of a text box, splitting `buffer` on raw `'\n'` (not
/// `.lines()` — that drops a trailing empty line, which matters when the
/// cursor sits on one) and highlighting the cursor's character in reverse
/// video, same visual treatment as `render_question`'s custom-input mode.
pub(super) fn push_text_box_lines(
    lines: &mut Vec<Line<'static>>,
    buffer: &str,
    cursor_chars: usize,
    theme: &Theme,
    max_rows: usize,
) {
    let rows: Vec<&str> = buffer.split('\n').collect();

    // Walk the whole buffer once, tracking (row, col) as a char-index
    // counter reaches `cursor_chars` — `\n` both advances the row and
    // resets the column, everything else advances the column.
    let (mut cursor_row, mut cursor_col) = (0usize, 0usize);
    for (i, ch) in buffer.chars().enumerate() {
        if i == cursor_chars {
            break;
        }
        if ch == '\n' {
            cursor_row += 1;
            cursor_col = 0;
        } else {
            cursor_col += 1;
        }
    }

    let max_rows = max_rows.max(1);
    let start = cursor_row.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(rows.len());

    for (i, row_text) in rows.iter().enumerate() {
        if i < start || i >= end {
            continue;
        }
        if i == cursor_row {
            let chars: Vec<char> = row_text.chars().collect();
            let before: String = chars.iter().take(cursor_col).collect();
            if cursor_col < chars.len() {
                let cursor_ch = chars[cursor_col].to_string();
                let after: String = chars.iter().skip(cursor_col + 1).collect();
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.fg)),
                    Span::styled(before, Style::default().fg(theme.fg).bg(theme.input_bg)),
                    Span::styled(
                        cursor_ch,
                        Style::default()
                            .fg(theme.bg)
                            .bg(theme.accent)
                            .add_modifier(Modifier::REVERSED),
                    ),
                    Span::styled(after, Style::default().fg(theme.fg).bg(theme.input_bg)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.fg)),
                    Span::styled(before, Style::default().fg(theme.fg).bg(theme.input_bg)),
                    Span::styled(
                        " ",
                        Style::default()
                            .fg(theme.fg)
                            .bg(theme.accent)
                            .add_modifier(Modifier::REVERSED),
                    ),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.fg)),
                Span::styled(
                    row_text.to_string(),
                    Style::default().fg(theme.fg).bg(theme.input_bg),
                ),
            ]));
        }
    }
}
