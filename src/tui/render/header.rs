//! Шапка экрана чата и линия подвала — пара зеркальных линий. Шапка начинается
//! надписью (продукт, версия, название чата) и тянется линией вправо; подвал
//! тянется линией и кончается надписью (`footer`).

use super::util::truncate;
use crate::app::AppState;
use crate::app::events::View;
use crate::provider::Role;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// Знак линии шапки и подвала.
pub(super) const RULE: &str = "\u{2500}";
/// Предел названия чата в шапке, в колонках.
const TITLE_MAX_COLS: usize = 60;
/// Короче названию не стоит: обрубок не читается.
const TITLE_MIN_COLS: usize = 8;
/// `· ` перед названием и пробел после него.
const TITLE_FRAME_COLS: usize = 3;
/// Линия справа от названия не короче двух клеток.
const RULE_MIN_COLS: usize = 2;

/// Название текущего чата: заголовок служебной модели, иначе первая строка
/// первой реплики. `None` — в чате ещё ничего нет.
pub(super) fn chat_title(state: &AppState) -> Option<String> {
    if state.view != View::Chat {
        return None;
    }
    let focused = state.focused();
    let title = focused.title.trim();
    if !title.is_empty() {
        return Some(title.to_string());
    }
    focused
        .messages
        .iter()
        .filter(|message| !message.ui_only && matches!(message.role, Role::User | Role::Assistant))
        .find_map(|message| {
            let text = message.visible_content();
            let line = text.trim().lines().next()?.trim().to_string();
            (!line.is_empty()).then_some(line)
        })
}

/// Строка шапки: ` pooprusteek 0.2 · название ────`.
pub(super) fn header_line(state: &AppState, width: usize, theme: &Theme) -> Line<'static> {
    let bg = Style::default().bg(theme.bg);
    let mut spans = vec![
        Span::styled(
            " pooprusteek",
            bg.fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", crate::update::CURRENT_VERSION),
            bg.fg(theme.text_dim),
        ),
    ];
    let mut used: usize = spans.iter().map(|s| s.content.width()).sum();
    if let Some(title) = chat_title(state) {
        // Название ужимается, линия справа остаётся хотя бы в две клетки.
        let room = width
            .saturating_sub(used + TITLE_FRAME_COLS + RULE_MIN_COLS)
            .min(TITLE_MAX_COLS);
        if room >= TITLE_MIN_COLS {
            let shown = format!("{} ", truncate(&title, room));
            spans.push(Span::styled("\u{00B7} ", bg.fg(theme.text_dim)));
            used += 2 + shown.width();
            spans.push(Span::styled(shown, bg.fg(theme.text_soft)));
        }
    }
    spans.push(Span::styled(
        RULE.repeat(width.saturating_sub(used)),
        bg.fg(theme.border),
    ));
    Line::from(spans)
}

pub(super) fn render_header(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(header_line(state, usize::from(area.width), theme))
            .style(Style::default().bg(theme.bg)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;
    use crate::provider::ChatMessage;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_header_fills_the_width_on_every_size() {
        let theme = Theme::default_dark();
        let mut state = AppState::new(Conversation::fresh_main(None));
        state.focused_mut().messages.push(ChatMessage::user(
            "почему падает items::range на пустой строке?",
        ));
        // Название продукта с версией не режется: уже него линия просто пропадает.
        let brand = format!(" pooprusteek {} ", crate::update::CURRENT_VERSION).width();
        for width in 10..160 {
            let line = header_line(&state, width, &theme);
            assert_eq!(text(&line).width(), width.max(brand), "width {width}");
        }
    }

    #[test]
    fn the_title_prefers_the_named_chat_over_the_first_message() {
        let mut state = AppState::new(Conversation::fresh_main(None));
        assert_eq!(chat_title(&state), None);
        state
            .focused_mut()
            .messages
            .push(ChatMessage::user("first line\nsecond"));
        assert_eq!(chat_title(&state).as_deref(), Some("first line"));
        state.focused_mut().title = "Починить тест".to_string();
        assert_eq!(chat_title(&state).as_deref(), Some("Починить тест"));
    }
}
