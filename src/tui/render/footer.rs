//! Нижняя строка: слева провайдер и модель, MCP, GOAL; справа заполнение окна
//! контекста и время. При нехватке места части сокращаются по приоритету
//! (`segments`), а не обрезаются как попало.

use super::segments::{Fitted, Segment, Tone, fit_segments, fitted_width};
use super::util::provider_label;
use crate::app::AppState;
use crate::app::events::GoalStage;
use crate::config::Config;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Клеток в шкале заполнения контекста.
const CTX_BAR_CELLS: usize = 8;
/// Сообщения состояния, которые не стоит повторять в подвале.
const QUIET_STATUSES: [&str; 4] = ["Ready", "Thinking...", "Tool finished", ""];

/// Шкала `███░░░░░` для `percent` процентов.
fn ctx_bar(percent: u8) -> String {
    let filled = (usize::from(percent.min(100)) * CTX_BAR_CELLS + 50) / 100;
    format!(
        "{}{}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(CTX_BAR_CELLS - filled)
    )
}

fn goal_label(state: &AppState) -> Option<String> {
    if !state.goal.mode {
        return None;
    }
    Some(match state.goal.stage {
        GoalStage::Inactive => "GOAL on".to_string(),
        GoalStage::WaitForGoal => "GOAL needs a goal".to_string(),
        GoalStage::RunAgent1 => format!("GOAL iter {}", state.goal.iteration),
        GoalStage::RunEvaluator => "GOAL eval".to_string(),
        GoalStage::Done => "GOAL done".to_string(),
    })
}

/// Стоит ли показывать текст состояния в подвале.
fn status_worth_showing(state: &AppState) -> bool {
    let message = state.status_message.trim();
    if QUIET_STATUSES.contains(&message) {
        return false;
    }
    // Пока идёт ход, «Running …» уже написано в панели работы.
    !(state.focused().generation.active && message.starts_with("Running "))
}

fn left_segments(state: &AppState, config: &Config, dock_visible: bool) -> Vec<Segment> {
    let model = config.active_model();
    let mut left = vec![
        Segment::new(
            1,
            Tone::Plain,
            format!(" {} \u{00B7} {model}", provider_label(config)),
        )
        .or_short(format!(" {model}")),
    ];
    if !dock_visible && let Some(goal) = goal_label(state) {
        left.push(Segment::new(2, Tone::Accent, format!(" \u{00B7} {goal}")));
    }
    if state.mcp_status.server_count > 0 {
        left.push(Segment::new(
            3,
            Tone::Dim,
            format!(
                " \u{00B7} mcp {}/{}",
                state.mcp_status.connected_count, state.mcp_status.server_count
            ),
        ));
    }
    let chats = state
        .conversations
        .iter()
        .filter(|conv| !conv.is_background_kind())
        .count();
    if chats > 1 {
        left.push(Segment::new(
            4,
            Tone::Dim,
            format!(" \u{00B7} chats {chats}"),
        ));
    }
    if !dock_visible {
        if state.background.total > 0 {
            left.push(Segment::new(
                3,
                Tone::Dim,
                format!(" \u{00B7} bg {}", state.background.total),
            ));
        }
        if state.timers_pending > 0 {
            left.push(Segment::new(
                4,
                Tone::Dim,
                format!(" \u{00B7} timers {}", state.timers_pending),
            ));
        }
    }
    if status_worth_showing(state) {
        let message = state.status_message.trim();
        left.push(
            Segment::new(2, Tone::Warning, format!(" \u{00B7} {message}")).or_short(format!(
                " \u{00B7} {}",
                crate::util::one_line_label(message, 24)
            )),
        );
    }
    left
}

fn right_segments(
    state: &AppState,
    config: &Config,
    dock_visible: bool,
    clock: &str,
) -> Vec<Segment> {
    let mut right = Vec::new();
    if !dock_visible && state.error_count > 0 {
        let glyph = crate::tui::glyphs::get().warn;
        let noun = if state.error_count == 1 {
            "error"
        } else {
            "errors"
        };
        right.push(
            Segment::new(
                0,
                Tone::Error,
                format!("{glyph} {} {noun}  ", state.error_count),
            )
            .or_short(format!("{glyph} {}  ", state.error_count)),
        );
    }
    // Заполнение окна показываем, только когда размер окна известен: выдуманный
    // процент от угаданного окна хуже, чем его отсутствие.
    // И пока в чате нет сообщений: на приветствии окно ещё ничем не занято.
    let budget = crate::context::spec::budget_from_config(config, state.provider_context_window);
    let snapshot = budget.snapshot(state.focused().context_used);
    if let Some(snapshot) = snapshot.filter(|_| !state.focused().messages.is_empty()) {
        let percent = snapshot.percent_used();
        let server = match &state.focused().provider {
            Some(provider) if provider.keeps_server_side_history() => " srv",
            _ => "",
        };
        right.push(
            Segment::new(
                2,
                Tone::Dim,
                format!("ctx {} {}{server}  ", ctx_bar(percent), snapshot.label()),
            )
            .or_short(format!("ctx {} {percent}%{server}  ", ctx_bar(percent)))
            .or_short(format!("ctx {percent}%  ")),
        );
    }
    right.push(Segment::new(5, Tone::Dim, format!("{clock} ")));
    right
}

fn tone_style(tone: Tone, theme: &Theme) -> Style {
    let base = Style::default().bg(theme.bg);
    match tone {
        Tone::Plain => base.fg(theme.text_soft),
        Tone::Dim => base.fg(theme.text_dim),
        Tone::Accent => base.fg(theme.accent).add_modifier(Modifier::BOLD),
        Tone::Warning => base.fg(theme.warning),
        Tone::Error => base.fg(theme.error).add_modifier(Modifier::BOLD),
    }
}

/// Подвал в клетках: слева группа, справа группа, между ними пустота.
pub(super) fn footer_parts(
    state: &AppState,
    config: &Config,
    width: usize,
    dock_visible: bool,
    clock: &str,
) -> (Vec<Fitted>, Vec<Fitted>) {
    fit_segments(
        width,
        left_segments(state, config, dock_visible),
        right_segments(state, config, dock_visible, clock),
    )
}

pub(super) fn render_footer(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
    dock_visible: bool,
) {
    if area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let clock = super::util::clock_label();
    let (left, right) = footer_parts(state, config, width, dock_visible, &clock);
    let gap = width.saturating_sub(fitted_width(&left) + fitted_width(&right));
    let mut spans: Vec<Span> = left
        .into_iter()
        .map(|part| Span::styled(part.text, tone_style(part.tone, theme)))
        .collect();
    spans.push(Span::styled(" ".repeat(gap), Style::default().bg(theme.bg)));
    spans.extend(
        right
            .into_iter()
            .map(|part| Span::styled(part.text, tone_style(part.tone, theme))),
    );
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

/// Одна строка состояния для экранов без подвала (онбординг): итог `/logout`
/// и `/wipe` приходит именно сюда.
pub(super) fn render_status_line(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", state.status_message),
            Style::default().fg(theme.text_dim).bg(theme.bg),
        ))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;
    use unicode_width::UnicodeWidthStr;

    fn state() -> AppState {
        AppState::new(Conversation::fresh_main(None))
    }

    fn line(parts: &(Vec<Fitted>, Vec<Fitted>)) -> String {
        parts
            .0
            .iter()
            .chain(parts.1.iter())
            .map(|part| part.text.as_str())
            .collect()
    }

    #[test]
    fn the_bar_fills_in_proportion() {
        assert_eq!(ctx_bar(0), "\u{2591}".repeat(8));
        assert_eq!(ctx_bar(100), "\u{2588}".repeat(8));
        assert_eq!(ctx_bar(38).chars().filter(|c| *c == '\u{2588}').count(), 3);
        assert_eq!(UnicodeWidthStr::width(ctx_bar(250).as_str()), 8);
    }

    #[test]
    fn the_footer_names_the_model_once() {
        let config = Config::default();
        let model = config.active_model().to_string();
        let parts = footer_parts(&state(), &config, 120, false, "14:02");
        assert_eq!(line(&parts).matches(model.as_str()).count(), 1);
        assert!(line(&parts).contains("14:02"));
    }

    #[test]
    fn the_footer_fits_every_width_and_keeps_the_error_marker() {
        let config = Config::default();
        let mut state = state();
        state.error_count = 3;
        for width in 20..160 {
            let parts = footer_parts(&state, &config, width, false, "14:02");
            let used = fitted_width(&parts.0) + fitted_width(&parts.1);
            // Защищённый маркер ошибок может быть шире совсем узкого подвала.
            assert!(used <= width.max(14), "width {width}: used {used}");
            assert!(
                line(&parts).contains('3'),
                "width {width}: the error count is protected"
            );
        }
    }

    #[test]
    fn the_error_marker_moves_to_the_dock_when_it_is_open() {
        let config = Config::default();
        let mut state = state();
        state.error_count = 2;
        let with_dock = footer_parts(&state, &config, 120, true, "14:02");
        assert!(!line(&with_dock).contains("errors"));
        let without = footer_parts(&state, &config, 120, false, "14:02");
        assert!(line(&without).contains("2 errors"));
    }

    #[test]
    fn a_quiet_status_is_not_repeated() {
        let mut state = state();
        state.status_message = "Ready".to_string();
        assert!(!status_worth_showing(&state));
        state.status_message = "Approve tool bash?".to_string();
        assert!(status_worth_showing(&state));
    }

    #[test]
    fn the_context_gauge_waits_for_the_first_message() {
        let mut config = Config::default();
        config.context.context_window = 32_000;
        let mut state = state();
        let empty = footer_parts(&state, &config, 120, false, "14:02");
        assert!(!line(&empty).contains("ctx"), "{}", line(&empty));
        state
            .focused_mut()
            .messages
            .push(crate::provider::ChatMessage::user("hi"));
        let started = footer_parts(&state, &config, 120, false, "14:02");
        assert!(line(&started).contains("ctx"), "{}", line(&started));
    }
}
