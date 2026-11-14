//! Подвал — линия-зеркало шапки: `─ модель · ctx 38% · 1 queued ──── провайдер ─`.
//! Слева то, что меняется по ходу работы, справа — кто отвечает (потом там же
//! будут лимиты). При нехватке места части сокращаются по приоритету
//! (`segments`), а не обрезаются как попало.

use super::header::RULE;
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

/// Сообщения состояния, которые не стоит повторять в подвале.
const QUIET_STATUSES: [&str; 4] = ["Ready", "Thinking...", "Tool finished", ""];
/// Клеток, которые подвал оставляет линии: края и хотя бы две в середине.
const RULE_COLS: usize = 4;

pub(super) fn goal_label(state: &AppState) -> Option<String> {
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

/// Кто отвечает сейчас: `● claude-max +2`, а у закреплённого чата — `pinned claude-max`.
/// Одного аккаунта и выключенного шлюза нет в подвале вовсе.
fn account_indicator(state: &AppState) -> Option<String> {
    let focused = state.conversations.focused_id();
    if let Some(pin) = state.gateway_view.pins.get(&focused) {
        return Some(format!("pinned {}", pin.label));
    }
    let (mark, _, label, others) = super::accounts::footer_account(&state.gateway_view)?;
    let glyph = super::accounts::footer_glyph(mark);
    Some(if others > 0 {
        format!("{glyph} {label} +{others}")
    } else {
        format!("{glyph} {label}")
    })
}

/// Модель, что отвечает при включённом шлюзе: у закреплённого чата своя, иначе по последнему выбору.
fn gateway_model(state: &AppState) -> Option<String> {
    use crate::app::gateway_view::rows::last_decision_model;
    let view = &state.gateway_view;
    let snapshot = view.snapshot.as_ref()?;
    if let Some(pin) = view.pins.get(&state.conversations.focused_id()) {
        return pin.model.clone().or_else(|| {
            snapshot
                .accounts
                .iter()
                .find(|a| a.id == pin.account)
                .and_then(|a| a.default_model.clone())
        });
    }
    last_decision_model(snapshot)
}

/// Модель, что отвечает: со шлюзом — по его выбору, без него — из конфига.
pub(super) fn model_label(state: &AppState, config: &Config) -> Option<String> {
    if state.gateway_view.enabled {
        gateway_model(state)
    } else {
        Some(config.active_model().to_string())
    }
}

/// Заполнение окна контекста. Пока размер окна не известен или в чате пусто,
/// не показываем: выдуманный процент хуже, чем его отсутствие.
fn context_segment(state: &AppState, config: &Config) -> Option<Segment> {
    if state.focused().messages.is_empty() {
        return None;
    }
    let budget = crate::context::spec::budget_from_config(config, state.provider_context_window);
    let snapshot = budget.snapshot(state.focused().context_used)?;
    let percent = snapshot.percent_used();
    let server = match &state.focused().provider {
        Some(provider) if provider.keeps_server_side_history() => " srv",
        _ => "",
    };
    Some(
        Segment::new(
            2,
            Tone::Dim,
            format!(" \u{00B7} ctx {}{server}", snapshot.label()),
        )
        .or_short(format!(" \u{00B7} ctx {percent}%{server}"))
        .or_short(format!(" \u{00B7} {percent}%")),
    )
}

/// Части слева. На главной модель уже подписана под полем, а контекста ещё нет.
fn left_segments(state: &AppState, config: &Config, work_shown: bool, home: bool) -> Vec<Segment> {
    let mut left = Vec::new();
    if !home {
        if let Some(model) = model_label(state, config) {
            left.push(Segment::new(1, Tone::Plain, format!(" {model}")));
        }
        left.extend(context_segment(state, config));
    }
    let queued = state.focused().queued.len();
    if queued > 0 {
        left.push(Segment::new(
            1,
            Tone::Accent,
            format!(" \u{00B7} {queued} queued"),
        ));
    }
    if !work_shown && let Some(goal) = goal_label(state) {
        left.push(Segment::new(2, Tone::Accent, format!(" \u{00B7} {goal}")));
    }
    if state.mcp_status.server_count > 0 {
        left.push(Segment::new(
            4,
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
    if !work_shown {
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
    // Первая часть не начинается с точки: на главной модели слева нет.
    if let Some(first) = left.first_mut() {
        for text in &mut first.variants {
            if let Some(rest) = text.strip_prefix(" \u{00B7} ") {
                *text = format!(" {rest}");
            }
        }
    }
    left.push(Segment::new(0, Tone::Dim, " "));
    left
}

/// Кто отвечает: со шлюзом — аккаунт (и единственный тоже), без него — провайдер из конфига.
fn provider_text(state: &AppState, config: &Config) -> String {
    if let Some(indicator) = account_indicator(state) {
        return indicator;
    }
    if state.gateway_view.enabled
        && let Some(row) = state.gateway_view.account_rows().into_iter().next()
    {
        return row.label;
    }
    provider_label(config)
}

/// Счётчик ошибок: защищён от сокращения, пока работу не показывает док или панель.
fn error_segment(state: &AppState, work_shown: bool) -> Option<Segment> {
    if work_shown || state.error_count == 0 {
        return None;
    }
    let glyph = crate::tui::glyphs::get().warn;
    let noun = if state.error_count == 1 {
        "error"
    } else {
        "errors"
    };
    Some(
        Segment::new(
            0,
            Tone::Error,
            format!(" {glyph} {} {noun}", state.error_count),
        )
        .or_short(format!(" {glyph} {}", state.error_count)),
    )
}

/// Справа: ошибки и кто отвечает. Со шлюзом это аккаунт, без него — провайдер.
fn right_segments(state: &AppState, config: &Config, work_shown: bool) -> Vec<Segment> {
    let mut right: Vec<Segment> = error_segment(state, work_shown).into_iter().collect();
    right.push(Segment::new(
        1,
        Tone::Plain,
        format!(" {} ", provider_text(state, config)),
    ));
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

/// Части подвала, уместившиеся между линиями по краям в `width` клеток.
pub(super) fn footer_parts(
    state: &AppState,
    config: &Config,
    width: usize,
    work_shown: bool,
) -> (Vec<Fitted>, Vec<Fitted>) {
    parts_for(state, config, width, work_shown, false)
}

fn parts_for(
    state: &AppState,
    config: &Config,
    width: usize,
    work_shown: bool,
    home: bool,
) -> (Vec<Fitted>, Vec<Fitted>) {
    fit_segments(
        width.saturating_sub(RULE_COLS),
        left_segments(state, config, work_shown, home),
        right_segments(state, config, work_shown),
    )
}

pub(super) fn render_footer(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
    work_shown: bool,
) {
    if area.height == 0 {
        return;
    }
    let parts = footer_parts(state, config, usize::from(area.width), work_shown);
    draw_rule(frame, area, parts, theme);
}

/// Нижняя линия главной: без модели и контекста, провайдер справа.
pub(super) fn render_home_footer(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let parts = parts_for(state, config, usize::from(area.width), false, true);
    draw_rule(frame, area, parts, theme);
}

/// Нижняя строка листа, без линий: папка · модель · провайдер и то, что меняется,
/// слева; ошибки и подсказка `hint` справа.
pub(super) fn render_sheet_footer(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
    hint: &str,
) {
    if area.height == 0 {
        return;
    }
    // Модель неизвестна, пока шлюз не выбрал аккаунт: пропускаем её вместе с точкой.
    let model = model_label(state, config)
        .map(|model| format!("{model} \u{00B7} "))
        .unwrap_or_default();
    let mut left = vec![
        Segment::new(
            3,
            Tone::Dim,
            format!(" {} \u{00B7} {model}", state.workspace_path),
        )
        .or_short(format!(" {model}")),
        Segment::new(1, Tone::Plain, provider_text(state, config)),
    ];
    left.extend(left_segments(state, config, false, true));
    let mut right: Vec<Segment> = error_segment(state, false).into_iter().collect();
    right.push(Segment::new(5, Tone::Dim, format!(" {hint} ")));
    let width = usize::from(area.width);
    let (left, right) = fit_segments(width, left, right);
    let gap = width.saturating_sub(fitted_width(&left) + fitted_width(&right));
    let mut spans: Vec<Span> = left
        .into_iter()
        .map(|part| Span::styled(part.text, tone_style(part.tone, theme)))
        .collect();
    spans.push(Span::raw(" ".repeat(gap)));
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

/// Линия `─ части ──── части ` ровно на ширину `area`.
fn draw_rule(frame: &mut Frame, area: Rect, parts: (Vec<Fitted>, Vec<Fitted>), theme: &Theme) {
    let width = usize::from(area.width);
    let (left, right) = parts;
    let rule = Style::default().fg(theme.border).bg(theme.bg);
    // Зеркало шапки: та начинается пробелом и кончается линией, подвал — наоборот.
    // Последняя клетка пустая ещё и потому, что угол экрана консоль рисует не всегда.
    let fill = width.saturating_sub(fitted_width(&left) + fitted_width(&right) + 1);
    let mut spans = vec![Span::styled(RULE, rule)];
    spans.extend(
        left.into_iter()
            .map(|part| Span::styled(part.text, tone_style(part.tone, theme))),
    );
    spans.push(Span::styled(RULE.repeat(fill), rule));
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
    fn the_footer_names_the_model_once() {
        let config = Config::default();
        let model = config.active_model().to_string();
        let parts = footer_parts(&state(), &config, 120, false);
        assert_eq!(line(&parts).matches(model.as_str()).count(), 1);
        assert!(line(&parts).contains(&provider_label(&config)));
    }

    #[test]
    fn the_footer_fits_every_width_and_keeps_the_error_marker() {
        let config = Config::default();
        let mut state = state();
        state.error_count = 3;
        for width in 20..160 {
            let parts = footer_parts(&state, &config, width, false);
            let used = fitted_width(&parts.0) + fitted_width(&parts.1);
            // Защищённый маркер ошибок может быть шире совсем узкого подвала.
            assert!(
                used <= width.saturating_sub(4).max(14),
                "width {width}: used {used}"
            );
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
        let with_dock = footer_parts(&state, &config, 120, true);
        assert!(!line(&with_dock).contains("errors"));
        let without = footer_parts(&state, &config, 120, false);
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
        let empty = footer_parts(&state, &config, 120, false);
        assert!(!line(&empty).contains("ctx"), "{}", line(&empty));
        state
            .focused_mut()
            .messages
            .push(crate::provider::ChatMessage::user("hi"));
        let started = footer_parts(&state, &config, 120, false);
        assert!(line(&started).contains("ctx"), "{}", line(&started));
    }
}
