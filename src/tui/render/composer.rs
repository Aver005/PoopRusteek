//! Правило над полем ввода: подпись слева и подсказки клавиш справа. Подсказки
//! зависят от того, что происходит сейчас, и сокращаются под ширину.

use crate::app::AppState;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

const RULE: &str = "\u{2500}";

/// Варианты подсказки клавиш от полного к самому короткому.
fn hint_variants(state: &AppState) -> &'static [&'static str] {
    if state.autocomplete.visible {
        &[
            "Tab/\u{2191}\u{2193} choose \u{00B7} Enter accept \u{00B7} Esc close",
            "Enter accept \u{00B7} Esc close",
            "Esc close",
        ]
    } else if state.focused().generation.active {
        &[
            "Esc cancel \u{00B7} Enter queue \u{00B7} Shift+Enter newline",
            "Esc cancel \u{00B7} Enter queue",
            "Esc cancel",
        ]
    } else {
        &[
            "Enter send \u{00B7} Shift+Enter newline \u{00B7} / commands \u{00B7} @ file",
            "Enter send \u{00B7} / commands",
            "Enter send",
        ]
    }
}

/// Подпись слева: что это за поле и сколько сообщений ждёт в очереди.
fn label(state: &AppState) -> String {
    match state.focused().queued.len() {
        0 => "message".to_string(),
        n => format!("message \u{00B7} {n} queued"),
    }
}

/// Строка правила нужной ширины: `── message ───── hints ─`.
pub(super) fn rule_text(state: &AppState, width: usize) -> (String, String, String) {
    let left = format!("{RULE}{RULE} {} ", label(state));
    let left_width = UnicodeWidthStr::width(left.as_str());
    if left_width >= width {
        // Совсем узко: подпись не помещается, остаётся голая линия.
        return (RULE.repeat(width), String::new(), String::new());
    }
    for hint in hint_variants(state) {
        let right = format!(" {hint} {RULE}");
        let right_width = UnicodeWidthStr::width(right.as_str());
        if left_width + right_width < width {
            let fill = RULE.repeat(width - left_width - right_width);
            return (left, fill, right);
        }
    }
    let fill = RULE.repeat(width.saturating_sub(left_width));
    (left, fill, String::new())
}

pub(super) fn render_rule(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let (left, fill, right) = rule_text(state, area.width as usize);
    let rule_style = Style::default().fg(theme.border).bg(theme.bg);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(RULE.repeat(2), rule_style),
            Span::styled(
                left.trim_start_matches(RULE).to_string(),
                Style::default().fg(theme.text_soft).bg(theme.bg),
            ),
            Span::styled(fill, rule_style),
            Span::styled(right, Style::default().fg(theme.text_dim).bg(theme.bg)),
        ])),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;

    fn state() -> AppState {
        AppState::new(Conversation::fresh_main(None))
    }

    #[test]
    fn the_rule_always_fills_the_width() {
        let state = state();
        for width in 10..140 {
            let (left, fill, right) = rule_text(&state, width);
            let total = UnicodeWidthStr::width(format!("{left}{fill}{right}").as_str());
            assert_eq!(total, width, "width {width}");
        }
    }

    #[test]
    fn hints_shrink_before_they_disappear() {
        let state = state();
        let (_, _, wide) = rule_text(&state, 120);
        let (_, _, narrow) = rule_text(&state, 40);
        let (_, _, tiny) = rule_text(&state, 14);
        assert!(wide.contains("Shift+Enter"));
        assert!(!narrow.is_empty() && !narrow.contains("Shift+Enter"));
        assert!(tiny.is_empty());
    }

    #[test]
    fn a_running_turn_advertises_cancel_and_queue() {
        let mut state = state();
        state.focused_mut().generation.active = true;
        let (_, _, right) = rule_text(&state, 100);
        assert!(right.contains("Esc cancel") && right.contains("Enter queue"));
    }

    #[test]
    fn the_label_counts_queued_messages() {
        let mut state = state();
        assert_eq!(label(&state), "message");
        state
            .focused_mut()
            .queued
            .push_back(crate::app::conversation::QueuedMessage {
                text: "next".to_string(),
                files: Vec::new(),
            });
        assert!(label(&state).contains("1 queued"));
    }
}
