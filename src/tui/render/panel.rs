//! Правая панель чата: всегда одной ширины ([`SIDEBAR_WIDTH`]), на всю высоту
//! между шапкой и подвалом. Секции: «Now» (работа и очередь), «Context» (окно,
//! GOAL, ошибки), «Accounts» (кто отвечает и лимиты), «Tools» (MCP, скиллы).
//! Секция без данных не рисуется.

use super::accounts::{gauge_bar, gauge_cells, mark_glyph, tone_color};
use super::dock::{self, DockModel};
use super::footer::goal_label;
use super::util::{provider_label, split_line, truncate};
use crate::app::AppState;
use crate::config::Config;
use crate::tui::glyphs;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

/// Отступ содержимого от разделителя и от правого края окна.
const PAD: u16 = 1;
/// Клеток в шкале окна контекста и в шкале лимита аккаунта.
const CONTEXT_CELLS: usize = 16;
const LIMIT_CELLS: usize = 8;
/// Сколько MCP-серверов перечисляем поимённо.
const SHOWN_SERVERS: usize = 6;
/// Место под « 83%» справа от шкалы контекста.
const PERCENT_COLS: usize = 5;
/// Сколько символов текста ошибки берём до урезания по клеткам.
const ERROR_PREVIEW_CHARS: usize = 200;

/// Строки панели для ширины `width` (без отступа).
pub(super) fn panel_lines(
    state: &AppState,
    config: &Config,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let sections = [
        now_section(state, width, theme),
        context_section(state, config, width, theme),
        accounts_section(state, config, width, theme),
        tools_section(state, config, width, theme),
    ];
    let mut lines = Vec::new();
    for (title, body) in sections.into_iter().flatten() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.text_soft)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(body);
    }
    lines
}

pub(super) fn render_panel(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
) {
    if area.width <= 2 * PAD || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    // Разделитель — в колонке слева от панели: раскладка оставляет её пустой.
    if let Some(sep_x) = area.x.checked_sub(1) {
        let sep = Style::default().fg(theme.border).bg(theme.bg);
        let column: Vec<Line> = (0..area.height)
            .map(|_| Line::from(Span::styled("\u{2502}", sep)))
            .collect();
        frame.render_widget(
            Paragraph::new(column),
            Rect::new(sep_x, area.y, 1, area.height),
        );
    }
    let inner = Rect::new(area.x + PAD, area.y, area.width - 2 * PAD, area.height);
    let lines = panel_lines(state, config, usize::from(inner.width), theme);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        inner,
    );
}

type Section = Option<(&'static str, Vec<Line<'static>>)>;

fn dim(theme: &Theme) -> Style {
    Style::default().fg(theme.text_dim)
}

/// Что делается сейчас: ход, суб-агенты, очередь, фон и таймеры.
fn now_section(state: &AppState, width: usize, theme: &Theme) -> Section {
    let model: DockModel = dock::model(state);
    let mut body = Vec::new();
    if let Some(main) = &model.main {
        body.push(split_line(
            vec![
                Span::styled(
                    format!("{} ", glyphs::get().spin(main.tick)),
                    Style::default().fg(theme.warning),
                ),
                Span::styled(dock::tool_text(&main.tool).0, Style::default().fg(theme.fg)),
            ],
            vec![Span::styled(dock::clock(main.elapsed_secs), dim(theme))],
            width,
        ));
        // Под строкой — суть вызова и скорость: при открытой панели дока нет.
        let (_, summary) = dock::tool_text(&main.tool);
        if !summary.is_empty() {
            body.push(Line::from(Span::styled(
                format!("  {}", truncate(&summary, width.saturating_sub(2))),
                Style::default().fg(theme.text_soft),
            )));
        }
        if main.tokens > 0 {
            let mut speed = format!("  {} tok", dock::compact_count(main.tokens));
            if main.tokens_per_sec > 0 {
                speed.push_str(&format!(" \u{00B7} {} t/s", main.tokens_per_sec));
            }
            body.push(Line::from(Span::styled(speed, dim(theme))));
        }
    }
    let last = model.children.len().saturating_sub(1);
    for (index, child) in model.children.iter().enumerate() {
        let branch = if index == last && model.hidden_agents == 0 {
            "\u{2514} "
        } else {
            "\u{251C} "
        };
        body.push(split_line(
            vec![
                Span::styled(branch, Style::default().fg(theme.border)),
                Span::styled(child.label.clone(), Style::default().fg(theme.text_soft)),
            ],
            vec![Span::styled(
                dock::clock(child.tool_secs.unwrap_or(child.elapsed_secs)),
                dim(theme),
            )],
            width,
        ));
    }
    if model.hidden_agents > 0 {
        body.push(Line::from(Span::styled(
            format!("\u{2514} +{} more agents", model.hidden_agents),
            dim(theme),
        )));
    }
    if let Some((count, next, paused)) = &model.queue {
        let lead = if *paused {
            format!("queue {count} paused \u{00B7} ")
        } else {
            format!("queue {count} \u{00B7} ")
        };
        body.push(split_line(
            vec![
                Span::styled(lead, Style::default().fg(theme.accent)),
                Span::styled(next.clone(), dim(theme)),
            ],
            Vec::new(),
            width,
        ));
    }
    let mut extra = Vec::new();
    if state.background.total > 0 {
        extra.push(format!("bg {}", state.background.total));
    }
    if state.timers_pending > 0 {
        extra.push(format!("timers {}", state.timers_pending));
    }
    if !extra.is_empty() {
        body.push(Line::from(Span::styled(
            extra.join(" \u{00B7} "),
            dim(theme),
        )));
    }
    (!body.is_empty()).then_some(("Now", body))
}

/// Окно контекста, GOAL и ошибки.
fn context_section(state: &AppState, config: &Config, width: usize, theme: &Theme) -> Section {
    let mut body = Vec::new();
    let budget = crate::context::spec::budget_from_config(config, state.provider_context_window);
    match budget.snapshot(state.focused().context_used) {
        Some(snapshot) => {
            let pct = f32::from(snapshot.percent_used());
            let cells = CONTEXT_CELLS.min(width.saturating_sub(PERCENT_COLS));
            let mut spans = gauge_bar(pct, cells, glyphs::get().gauge_full, theme.accent, theme);
            spans.push(Span::styled(
                format!(" {}%", snapshot.percent_used()),
                dim(theme),
            ));
            body.push(Line::from(spans));
        }
        None => body.push(Line::from(Span::styled("window size unknown", dim(theme)))),
    }
    if let Some(goal) = goal_label(state) {
        body.push(Line::from(Span::styled(
            goal,
            Style::default().fg(theme.accent),
        )));
    }
    if state.error_count > 0 {
        let noun = if state.error_count == 1 {
            "error"
        } else {
            "errors"
        };
        body.push(Line::from(Span::styled(
            format!("{} {} {noun}", glyphs::get().warn, state.error_count),
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(message) = &state.last_error {
            body.push(Line::from(Span::styled(
                truncate(
                    &crate::util::one_line_label(message, ERROR_PREVIEW_CHARS),
                    width,
                ),
                Style::default().fg(theme.error),
            )));
        }
    }
    Some(("Context", body))
}

/// Кто может отвечать и сколько у него осталось. Без шлюза — один провайдер.
fn accounts_section(state: &AppState, config: &Config, width: usize, theme: &Theme) -> Section {
    let rows = state.gateway_view.account_rows();
    if !state.gateway_view.enabled || rows.is_empty() {
        let line = split_line(
            vec![Span::styled(
                provider_label(config),
                Style::default().fg(theme.fg),
            )],
            vec![Span::styled(config.active_model().to_string(), dim(theme))],
            width,
        );
        return Some(("Accounts", vec![line]));
    }
    let body = rows
        .iter()
        .map(|row| {
            let lead = vec![
                Span::styled(
                    format!("{} ", mark_glyph(row.status.mark)),
                    Style::default().fg(tone_color(row.status.tone, theme)),
                ),
                Span::styled(row.label.clone(), Style::default().fg(theme.fg)),
            ];
            // Самое занятое окно лимита: именно оно кончится первым.
            let busiest = row
                .limits
                .iter()
                .filter(|limit| limit.pct.is_some())
                .max_by(|a, b| {
                    a.pct
                        .partial_cmp(&b.pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            match busiest {
                Some(limit) => split_line(lead, gauge_cells(limit, LIMIT_CELLS, theme), width),
                None => split_line(
                    lead,
                    vec![Span::styled(row.status.text.clone(), dim(theme))],
                    width,
                ),
            }
        })
        .collect();
    Some(("Accounts", body))
}

/// MCP-серверы и включённые скиллы.
fn tools_section(state: &AppState, config: &Config, width: usize, theme: &Theme) -> Section {
    let servers: Vec<_> = state
        .mcp_status
        .view
        .servers
        .iter()
        .filter(|server| server.enabled)
        .collect();
    let skills = config.skills.enabled.len();
    if servers.is_empty() && skills == 0 {
        return None;
    }
    let g = glyphs::get();
    let mut body = Vec::new();
    if !servers.is_empty() {
        body.push(split_line(
            vec![Span::styled("MCP", Style::default().fg(theme.fg))],
            vec![Span::styled(
                format!(
                    "{}/{}",
                    state.mcp_status.connected_count, state.mcp_status.server_count
                ),
                dim(theme),
            )],
            width,
        ));
        for server in servers.iter().take(SHOWN_SERVERS) {
            let ok = server.status == "connected";
            let (glyph, color) = if ok {
                (g.check, theme.success)
            } else {
                (g.warn, theme.warning)
            };
            body.push(split_line(
                vec![
                    Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                    Span::styled(server.name.clone(), Style::default().fg(theme.text_soft)),
                ],
                vec![Span::styled(server.tool_count.to_string(), dim(theme))],
                width,
            ));
        }
        if servers.len() > SHOWN_SERVERS {
            body.push(Line::from(Span::styled(
                format!("  +{} more", servers.len() - SHOWN_SERVERS),
                dim(theme),
            )));
        }
    }
    if skills > 0 {
        body.push(split_line(
            vec![Span::styled("skills", Style::default().fg(theme.fg))],
            vec![Span::styled(skills.to_string(), dim(theme))],
            width,
        ));
    }
    Some(("Tools", body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;
    use crate::provider::ChatMessage;

    fn widths(lines: &[Line]) -> Vec<usize> {
        lines.iter().map(Line::width).collect()
    }

    #[test]
    fn no_line_is_wider_than_the_panel() {
        let theme = Theme::default_dark();
        let config = Config::default();
        let mut state = AppState::new(Conversation::fresh_main(None));
        state.focused_mut().messages.push(ChatMessage::user("hi"));
        state.focused_mut().generation.active = true;
        state.error_count = 2;
        state.last_error = Some("a very long error message that never fits the side panel".into());
        let width = usize::from(super::super::layout::SIDEBAR_WIDTH - 2 * PAD);
        for w in widths(&panel_lines(&state, &config, width, &theme)) {
            assert!(w <= width, "{w} > {width}");
        }
    }

    #[test]
    fn an_idle_chat_has_no_now_section() {
        let theme = Theme::default_dark();
        let config = Config::default();
        let state = AppState::new(Conversation::fresh_main(None));
        let text: Vec<String> = panel_lines(&state, &config, 29, &theme)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(!text.iter().any(|l| l == "Now"), "{text:?}");
        assert!(text.iter().any(|l| l == "Context"));
        assert!(text.iter().any(|l| l == "Accounts"));
    }

    #[test]
    fn a_running_turn_shows_up_under_now() {
        let theme = Theme::default_dark();
        let config = Config::default();
        let mut state = AppState::new(Conversation::fresh_main(None));
        state.focused_mut().generation.active = true;
        let text: Vec<String> = panel_lines(&state, &config, 29, &theme)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text.first().map(String::as_str), Some("Now"));
        assert!(text[1].contains("thinking"), "{text:?}");
    }
}
