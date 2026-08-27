//! Каркас полноэкранных панелей (`/mcp`, `/providers`, `/themes`): рамка
//! заголовка сверху, подсказка по клавишам снизу, тело между ними. Три вида
//! рисовали его порознь одинаковыми копиями.

use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Разбить площадь на заголовок/тело/подвал, нарисовать заголовок и вернуть
/// области тела и подвала. Подвал рисуется позже: его подсказка зависит от
/// состояния, которое вид определяет уже после тела.
pub(super) fn panel_frame(
    frame: &mut Frame,
    area: Rect,
    title: impl Into<Line<'static>>,
    subtitle: impl Into<String>,
    theme: &Theme,
) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);

    let header = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.accent));
    let header_inner = header.inner(chunks[0]);
    frame.render_widget(header, chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            subtitle.into(),
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        header_inner,
    );

    (chunks[1], chunks[2])
}

/// Подвал панели. Рамка сначала, текст в её внутреннюю строку: обратный
/// порядок красил подсказку по строке рамки, и рамка её затирала.
pub(super) fn panel_footer(frame: &mut Frame, area: Rect, hint: impl Into<String>, theme: &Theme) {
    let footer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = footer.inner(area);
    frame.render_widget(footer, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint.into(),
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        inner,
    );
}
