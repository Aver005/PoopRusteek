//! Приветствие на месте переписки, пока в чате нет сообщений: карточка с
//! версией, моделью и папкой, затем недавние сессии и подсказки. Поле ввода
//! здесь не рисуется — оно общее со всеми экранами и стоит внизу (`layout`).

use super::onboarding::pulsing_title;
use super::segments::{Segment, Tone, fit_segments};
use super::util::{fit_col, format_date, provider_label, truncate};
use crate::app::AppState;
use crate::app::landing::SHOWN_SESSIONS;
use crate::config::Config;
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

/// Карточка: две строки текста и рамка.
const CARD_H: u16 = 4;
/// Ниже этого окна сессий нет места, показываем только заголовок.
const MODEL_COL_MIN_W: usize = 70;
const MODEL_W: usize = 16;
const DATE_W: usize = 12;
const GAP: usize = 2;

/// Что умещается в `height` строк: карточка, сессии, подсказки.
#[derive(Debug, PartialEq, Eq)]
struct WelcomePlan {
    card: bool,
    session_rows: usize,
    tips: bool,
}

/// Разделить высоту между частями. Карточка важнее сессий, сессии — подсказок.
fn plan(height: u16, sessions: usize) -> WelcomePlan {
    let height = usize::from(height);
    let card_rows = if height >= usize::from(CARD_H) + 2 {
        usize::from(CARD_H)
    } else {
        1
    };
    let card = card_rows == usize::from(CARD_H);
    let mut left = height.saturating_sub(card_rows);
    // Подсказки — последняя строка области, прямо над полем ввода.
    let tips = left >= 2;
    if tips {
        left -= 1;
    }
    // Пустая строка перед сессиями и их заголовок.
    let session_rows = if sessions > 0 && left >= 3 {
        (left - 2).min(sessions.min(SHOWN_SESSIONS))
    } else {
        0
    };
    WelcomePlan {
        card,
        session_rows,
        tips,
    }
}

fn card_lines(
    state: &AppState,
    config: &Config,
    inner_w: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut title = vec![Span::raw(" ")];
    title.extend(pulsing_title(
        state.focused().generation.animation_tick,
        theme,
    ));
    title.push(Span::styled(
        format!("  {}", crate::update::CURRENT_VERSION),
        Style::default().fg(theme.text_dim).bg(theme.bg),
    ));

    let mcp = if state.mcp_status.server_count > 0 {
        format!(
            " \u{00B7} mcp {}/{}",
            state.mcp_status.connected_count, state.mcp_status.server_count
        )
    } else {
        String::new()
    };
    let left = format!(
        " {} \u{00B7} {}{mcp}",
        provider_label(config),
        config.active_model()
    );
    let cwd = state.workspace_path.as_str();
    let left_w = UnicodeWidthStr::width(left.as_str());
    let room = inner_w.saturating_sub(left_w + 3);
    let mut info = vec![Span::styled(
        left,
        Style::default().fg(theme.text_soft).bg(theme.bg),
    )];
    if !cwd.is_empty() && room >= 12 {
        let shown = truncate(cwd, room);
        let gap = inner_w.saturating_sub(left_w + UnicodeWidthStr::width(shown.as_str()) + 1);
        info.push(Span::raw(" ".repeat(gap)));
        info.push(Span::styled(
            shown,
            Style::default().fg(theme.text_dim).bg(theme.bg),
        ));
    }
    vec![Line::from(title), Line::from(info)]
}

fn session_rows(state: &AppState, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let with_model = width >= MODEL_COL_MIN_W;
    let fixed = 2 + GAP + DATE_W + 1 + if with_model { MODEL_W + GAP } else { 0 };
    let title_w = width.saturating_sub(fixed).max(8);
    let mut lines = vec![Line::from(Span::styled(
        " Recent sessions",
        Style::default().fg(theme.text_dim).bg(theme.bg),
    ))];
    for session in state.landing.recent().iter().take(rows) {
        let mut spans = vec![
            Span::raw("   "),
            Span::styled(
                fit_col(&session.title, title_w),
                Style::default().fg(theme.fg).bg(theme.bg),
            ),
            Span::raw(" ".repeat(GAP)),
        ];
        if with_model {
            spans.push(Span::styled(
                fit_col(&session.model_type, MODEL_W),
                Style::default().fg(theme.accent_soft).bg(theme.bg),
            ));
            spans.push(Span::raw(" ".repeat(GAP)));
        }
        spans.push(Span::styled(
            fit_col(&format_date(&session.updated_at), DATE_W),
            Style::default().fg(theme.text_dim).bg(theme.bg),
        ));
        lines.push(Line::from(spans));
    }
    lines
}

fn tips_line(width: usize, theme: &Theme) -> Line<'static> {
    let dot = "\u{00B7}";
    let tips = vec![
        Segment::new(1, Tone::Dim, format!("/ commands {dot} ")),
        Segment::new(2, Tone::Dim, format!("@file attach {dot} ")),
        Segment::new(3, Tone::Dim, "/sessions open one"),
    ];
    let (parts, _) = fit_segments(width.saturating_sub(2), tips, Vec::new());
    let mut text: String = parts.iter().map(|part| part.text.as_str()).collect();
    // Висящий разделитель на конце не нужен.
    if let Some(stripped) = text.strip_suffix(&format!(" {dot} ")) {
        text = stripped.to_string();
    }
    Line::from(Span::styled(
        format!(" {text}"),
        Style::default().fg(theme.text_dim).bg(theme.bg),
    ))
}

/// Нарисовать приветствие в области переписки.
pub(super) fn render_welcome(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let content_w = area.width.saturating_sub(2);
    let content = Rect::new(area.x + 1, area.y, content_w, area.height);
    let plan = plan(area.height, state.landing.recent().len());
    let width = usize::from(content.width);

    let mut y = content.y;
    if plan.card {
        let border_type = if glyphs::get().ascii {
            BorderType::Plain
        } else {
            BorderType::Rounded
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(Style::default().fg(theme.border));
        let card = Rect::new(content.x, y, content.width, CARD_H);
        let inner = block.inner(card);
        frame.render_widget(block, card);
        frame.render_widget(
            Paragraph::new(card_lines(state, config, usize::from(inner.width), theme))
                .style(Style::default().bg(theme.bg)),
            inner,
        );
        y += CARD_H;
    } else {
        let line = card_lines(state, config, width, theme).remove(0);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.bg)),
            Rect::new(content.x, y, content.width, 1),
        );
        y += 1;
    }

    if plan.session_rows > 0 {
        y += 1;
        let lines = session_rows(state, plan.session_rows, width, theme);
        let height = lines.len() as u16;
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme.bg)),
            Rect::new(content.x, y, content.width, height),
        );
    }
    if plan.tips && area.height > 0 {
        // Пустота между сессиями и подсказками — честная: ввод всегда внизу.
        frame.render_widget(
            Paragraph::new(tips_line(width, theme)),
            Rect::new(content.x, area.bottom() - 1, content.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tall_area_fits_everything() {
        let plan = plan(24, 4);
        assert_eq!(
            plan,
            WelcomePlan {
                card: true,
                session_rows: 4,
                tips: true
            }
        );
    }

    #[test]
    fn a_short_area_drops_sessions_before_the_card() {
        let short = plan(6, 4);
        assert!(short.card && short.session_rows == 0);
        let tiny = plan(3, 4);
        assert!(!tiny.card);
    }

    #[test]
    fn the_plan_never_asks_for_more_rows_than_it_has() {
        for height in 0..40u16 {
            for sessions in 0..8 {
                let plan = plan(height, sessions);
                let card = if plan.card { usize::from(CARD_H) } else { 1 };
                let sessions_block = if plan.session_rows > 0 {
                    plan.session_rows + 2
                } else {
                    0
                };
                let tips = usize::from(plan.tips);
                let used = card + sessions_block + tips;
                assert!(
                    used <= usize::from(height).max(1),
                    "height {height}, sessions {sessions}, used {used}"
                );
            }
        }
    }

    #[test]
    fn at_most_the_shown_number_of_sessions_is_planned() {
        assert!(plan(60, 20).session_rows <= SHOWN_SESSIONS);
    }
}
