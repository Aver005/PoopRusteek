//! Рамка поля ввода. В простое она пустая; когда что-то происходит (идёт ход,
//! ждёт очередь, открыто автодополнение, настраивается GOAL), справа в верхней
//! границе — подсказка клавиш, сокращённая под ширину.

use crate::app::AppState;
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};
use unicode_width::UnicodeWidthStr;

/// Клеток рамки, которые подсказка не занимает: углы и линия по краям.
const HINT_MARGIN: usize = 6;

/// Варианты подсказки от полного к самому короткому. Пусто — в простое.
fn hint_variants(state: &AppState) -> &'static [&'static str] {
    if state.autocomplete.visible {
        &[
            "Tab/\u{2191}\u{2193} choose \u{00B7} Enter accept \u{00B7} Esc close",
            "Enter accept \u{00B7} Esc close",
            "Esc close",
        ]
    } else if state.goal.is_setup_pending() {
        &[
            "Enter send \u{00B7} Esc cancel GOAL setup",
            "Esc cancel GOAL setup",
            "Esc cancel",
        ]
    } else if state.focused().generation.active {
        &[
            "Esc cancel \u{00B7} Enter queue \u{00B7} Shift+Enter newline",
            "Esc cancel \u{00B7} Enter queue",
            "Esc cancel",
        ]
    } else if !state.focused().queued.is_empty() {
        // Очередь стоит без хода: подсказка про то, как с ней быть.
        &[
            "Enter send next queued \u{00B7} Esc return queued to input",
            "Enter send next \u{00B7} Esc return to input",
            "Esc return queued",
        ]
    } else {
        &[]
    }
}

/// Подсказка, что помещается в рамку шириной `width`.
pub(super) fn box_hint(state: &AppState, width: usize) -> Option<&'static str> {
    hint_variants(state)
        .iter()
        .copied()
        .find(|hint| UnicodeWidthStr::width(*hint) + HINT_MARGIN <= width)
}

/// Нарисовать рамку вокруг поля ввода. Само поле рисует `widgets::input`.
pub(super) fn render_input_box(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height < 2 || area.width < 4 {
        return;
    }
    let border_type = if glyphs::get().ascii {
        BorderType::Plain
    } else {
        BorderType::Rounded
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        // Поле — главное на экране: рамка заметнее линий шапки и подвала.
        .border_style(Style::default().fg(theme.text_dim).bg(theme.bg))
        .style(Style::default().bg(theme.input_bg));
    if let Some(hint) = box_hint(state, usize::from(area.width)) {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" {hint} "),
                Style::default().fg(theme.text_dim).bg(theme.bg),
            ))
            .alignment(Alignment::Right),
        );
    }
    frame.render_widget(block, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;

    fn state() -> AppState {
        AppState::new(Conversation::fresh_main(None))
    }

    #[test]
    fn an_idle_box_has_no_hint() {
        assert_eq!(box_hint(&state(), 120), None);
    }

    #[test]
    fn hints_shrink_before_they_disappear() {
        let mut state = state();
        state.focused_mut().generation.active = true;
        let wide = box_hint(&state, 120).expect("wide");
        let narrow = box_hint(&state, 30).expect("narrow");
        assert!(wide.contains("Shift+Enter"));
        assert!(!narrow.contains("Shift+Enter"));
        assert_eq!(box_hint(&state, 10), None);
    }

    #[test]
    fn a_running_turn_advertises_cancel_and_queue() {
        let mut state = state();
        state.focused_mut().generation.active = true;
        let hint = box_hint(&state, 100).expect("hint");
        assert!(hint.contains("Esc cancel") && hint.contains("Enter queue"));
    }

    #[test]
    fn a_waiting_queue_says_how_to_send_or_return_it() {
        let mut state = state();
        state
            .focused_mut()
            .queued
            .push_back(crate::app::conversation::QueuedMessage {
                text: "next".to_string(),
                chips: Vec::new(),
                named: Vec::new(),
            });
        let hint = box_hint(&state, 120).expect("hint");
        assert!(
            hint.contains("Enter send next") && hint.contains("Esc return"),
            "{hint}"
        );
    }
}
