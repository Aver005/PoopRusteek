//! Строка сессии в списках главной и `/resume`:
//! `▸ 12  название ……  модель  4 msgs  Nov 05`. Выбранная — с подсветкой.

use super::util::{SHORT_DATE_COLS, fit_col, short_date};
use crate::session::SessionSummary;
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const MODEL_COLS: usize = 16;
/// `123 msgs` и пробел перед ним.
const MESSAGES_COLS: usize = 10;

/// Какие колонки есть в строке и сколько места под номер и хвост.
#[derive(Debug, Clone, Copy)]
pub(super) struct RowLayout {
    /// Цифр в номере: у короткого списка одна, у длинного две.
    pub number_cols: usize,
    pub model: bool,
    pub messages: bool,
    /// Пустых клеток справа: под полосу прокрутки или просто отступ.
    pub trail: usize,
}

/// Одна строка шириной `width`. `number` — номер, который видит человек (с единицы).
pub(super) fn session_line(
    number: usize,
    session: &SessionSummary,
    picked: bool,
    width: usize,
    layout: RowLayout,
    today: chrono::NaiveDate,
    theme: &Theme,
) -> Line<'static> {
    let bg = if picked { theme.selection } else { theme.bg };
    let marker = if picked { glyphs::get().pointer } else { " " };
    let lead = format!(" {marker} {number:>w$} ", w = layout.number_cols);
    let warn = if session.broken {
        format!("{} ", glyphs::get().warn)
    } else {
        String::new()
    };
    let fixed = lead.width()
        + warn.width()
        + 1
        + SHORT_DATE_COLS
        + layout.trail
        + if layout.model { MODEL_COLS } else { 0 }
        + if layout.messages { MESSAGES_COLS } else { 0 };
    let title_w = width.saturating_sub(fixed);
    let dim = Style::default().fg(theme.text_dim).bg(bg);

    let mut spans = vec![
        Span::styled(lead, Style::default().fg(theme.accent).bg(bg)),
        Span::styled(warn, Style::default().fg(theme.warning).bg(bg)),
        Span::styled(
            fit_col(&session.title, title_w),
            Style::default()
                .fg(if picked { theme.fg } else { theme.text_soft })
                .bg(bg),
        ),
        Span::styled(" ", Style::default().bg(bg)),
    ];
    if layout.model {
        spans.push(Span::styled(fit_col(&session.model_type, MODEL_COLS), dim));
    }
    if layout.messages {
        let count = format!("{} msgs", session.message_count);
        spans.push(Span::styled(
            fit_col(
                &format!("{count:>w$} ", w = MESSAGES_COLS - 1),
                MESSAGES_COLS,
            ),
            dim,
        ));
    }
    spans.push(Span::styled(
        fit_col(
            &format!(
                "{:>SHORT_DATE_COLS$}",
                short_date(&session.updated_at, today)
            ),
            SHORT_DATE_COLS,
        ),
        dim,
    ));
    spans.push(Span::styled(
        " ".repeat(layout.trail),
        Style::default().bg(bg),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(title: &str, broken: bool) -> SessionSummary {
        SessionSummary {
            id: "x".into(),
            created_at: String::new(),
            updated_at: "2026-11-05T09:12:00+00:00".into(),
            workspace_root: String::new(),
            message_count: 12,
            title: title.into(),
            model_type: "deepseek-chat".into(),
            tag: None,
            broken,
        }
    }

    #[test]
    fn a_row_is_exactly_as_wide_as_asked_with_every_column() {
        let theme = Theme::default_dark();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        for (model, messages) in [(false, false), (true, true)] {
            let layout = RowLayout {
                number_cols: 2,
                model,
                messages,
                trail: 2,
            };
            for broken in [false, true] {
                let row = session_line(
                    12,
                    &session("Починить падающий тест items::range", broken),
                    true,
                    90,
                    layout,
                    today,
                    &theme,
                );
                assert_eq!(row.width(), 90, "{model} {messages} {broken}");
                let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(text.contains("12 msgs"), messages, "{text}");
            }
        }
    }
}
