//! The chrome around the chat view: horizontal separators, the input
//! border, the attach bar, and the bottom mini status bar.

use super::util::{provider_label, status_bar_gap};
use crate::app::AppState;
use crate::config::Config;
use crate::tui::theme::Theme;
use crate::tui::view_model;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(super) fn render_separator(frame: &mut Frame, area: Rect, theme: &Theme) {
    let line = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            line,
            Style::default().fg(theme.border).bg(theme.bg),
        ))),
        area,
    );
}

pub(super) fn render_input_border(frame: &mut Frame, area: Rect, theme: &Theme) {
    let w = area.width.saturating_sub(3) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}{}", "   ", "─".repeat(w)),
            Style::default().fg(theme.border).bg(theme.input_bg),
        ))),
        area,
    );
}

pub(super) fn render_mini_status(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
    // When the stats panel is hidden it can't carry the error marker, so the
    // status bar shows a compact red segment instead.
    panel_shown: bool,
) {
    let provider_name = provider_label(config);
    let model = config.active_model();
    let msg_count = state.focused().messages.len();
    let total_tokens = view_model::assistant_token_total(&state.focused().messages);

    let mcp_status = view_model::mcp_label(&state.mcp_status);
    let bg_status = if state.background.total > 0 {
        let mut suffix = String::new();
        if state.background.interactive > 0 {
            suffix.push_str(&format!("/{}i", state.background.interactive));
        }
        if state.background.persistent > 0 {
            suffix.push_str(&format!("/{}p", state.background.persistent));
        }
        format!(" bg:{}{}", state.background.total, suffix)
    } else {
        String::new()
    };
    let btw_status = {
        use crate::app::conversation::ConversationKind;
        let running = state
            .conversations
            .iter()
            .filter(|c| {
                (c.kind == ConversationKind::Sidechat || c.kind == ConversationKind::SubAgent)
                    && c.is_streaming()
            })
            .count();
        if running > 0 {
            format!(" agents:{running}")
        } else {
            String::new()
        }
    };
    let chats_status = {
        use crate::app::conversation::ConversationKind;
        // Switchable parallel chats = main + session conversations (not agents).
        let sessions = state
            .conversations
            .iter()
            .filter(|c| {
                c.kind != ConversationKind::Sidechat && c.kind != ConversationKind::SubAgent
            })
            .count();
        if sessions > 1 {
            format!(" chats:{sessions}")
        } else {
            String::new()
        }
    };

    let model_tag = if !state.focused().generation.last_model.is_empty() {
        format!(" · {}", state.focused().generation.last_model)
    } else {
        String::new()
    };
    let goal_tag = if state.goal.mode {
        format!(
            " GOAL:{} ",
            match state.goal.stage {
                crate::app::events::GoalStage::Inactive => "standby",
                crate::app::events::GoalStage::WaitForGoal => "need-goal",
                crate::app::events::GoalStage::RunAgent1 => "build",
                crate::app::events::GoalStage::RunEvaluator => "eval",
                crate::app::events::GoalStage::Done => "done",
            }
        )
    } else {
        String::new()
    };
    let left = format!(
        " {}{} · {}{}{}{}{}{} ",
        goal_tag, provider_name, model, model_tag, mcp_status, bg_status, btw_status, chats_status
    );

    let status_tag = state
        .focused()
        .generation
        .last_status
        .as_deref()
        .unwrap_or("");

    let center = if state.focused().generation.active {
        if let Some(start) = state.focused().generation.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            let tps = view_model::tokens_per_sec(state.focused().generation.last_tokens, elapsed);
            format!(
                " {} {} ",
                view_model::spinner_char(state.focused().generation.animation_tick),
                state.status_message
            ) + &format!(
                "({} tok, {:.1}s, {:.0} t/s)",
                state.focused().generation.last_tokens,
                elapsed,
                tps
            )
        } else {
            format!(
                " {} {} ",
                view_model::spinner_char(state.focused().generation.animation_tick),
                state.status_message
            )
        }
    } else {
        let mut parts = vec![state.status_message.clone()];
        if !status_tag.is_empty() {
            parts.push(status_tag.to_string());
        }
        let tps = view_model::tokens_per_sec(
            state.focused().generation.last_tokens,
            state.focused().generation.last_duration_secs,
        );
        if tps > 0.0 {
            parts.push(format!(
                "{} tok in {:.1}s ({:.0} t/s)",
                state.focused().generation.last_tokens,
                state.focused().generation.last_duration_secs,
                tps
            ));
        }
        format!(" {} ", parts.join(" · "))
    };

    let goal_tag = if state.goal.mode {
        let stage_str = match state.goal.stage {
            crate::app::events::GoalStage::Inactive => "ON".to_string(),
            crate::app::events::GoalStage::WaitForGoal => "NEED-GOAL".to_string(),
            crate::app::events::GoalStage::RunAgent1 => format!("iter#{}", state.goal.iteration),
            crate::app::events::GoalStage::RunEvaluator => "EVAL".to_string(),
            crate::app::events::GoalStage::Done => "DONE".to_string(),
        };
        format!(" GOAL:{} ", stage_str)
    } else {
        String::new()
    };
    let session_label = format!(
        " {}",
        view_model::session_label(&state.focused().session_id)
    );
    // Window fullness. Absent whenever the window size is unknown — showing a
    // plausible-looking percentage against a guessed window is how Goose let a
    // session run 36% past its limit with the indicator still looking fine.
    let mut budget = crate::context::ContextBudget::from_config(
        config.context.context_window,
        config.context.reserved_tokens,
    );
    budget.learn_provider_window(state.provider_context_window);
    // `srv` = the provider keeps the history, so the number is what the server
    // has accumulated for the session, not what we send each step.
    let ctx_mode = match &state.focused().provider {
        Some(provider) if provider.keeps_server_side_history() => " srv",
        _ => "",
    };
    let ctx_tag = budget
        .snapshot(state.focused().context_used)
        .map(|budget| format!(" ctx:{}{}", budget.label(), ctx_mode))
        .unwrap_or_default();
    let right = format!(
        "{} msgs:{} tot:{}{} | {} ",
        goal_tag, msg_count, total_tokens, ctx_tag, session_label
    );

    // Red error marker — only here when the panel (its primary home) is hidden.
    let err_text = if !panel_shown && state.error_count > 0 {
        format!(" \u{26A0} {} err ", state.error_count)
    } else {
        String::new()
    };

    // The marker sits between center and right; reserve its width in the gap.
    let gap = status_bar_gap(&left, &center, &format!("{err_text}{right}"), area.width);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(theme.text_dim).bg(theme.bg)),
            Span::styled(" ".repeat(gap), Style::default().bg(theme.bg)),
            Span::styled(
                center,
                if state.focused().generation.active {
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                }
                .bg(theme.bg),
            ),
            Span::styled(
                err_text,
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD)
                    .bg(theme.bg),
            ),
            Span::styled(right, Style::default().fg(theme.text_dim).bg(theme.bg)),
        ])),
        area,
    );
}

pub(super) fn render_attach_bar(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let files = &state.attached_files;
    if files.is_empty() {
        return;
    }
    let mut spans: Vec<Span> = vec![Span::styled(
        " \u{1F4CE} ",
        Style::default().fg(theme.accent_soft).bg(theme.input_bg),
    )];
    let max_w = area.width.saturating_sub(4) as usize;
    let mut remaining = max_w;
    for (i, f) in files.iter().enumerate() {
        let icon = if f.is_image { "\u{1F5BC}" } else { "\u{1F4C4}" };
        let size_str = crate::util::format_size(f.size);
        let label = format!(" {} {} {} ", icon, f.display_name, size_str);
        let sep = if i > 0 { " " } else { "" };
        let entry = format!("{}{}", sep, label);
        if entry.chars().count() > remaining {
            if spans.len() > 1 {
                spans.push(Span::styled(
                    " \u{2026}",
                    Style::default().fg(theme.text_dim).bg(theme.input_bg),
                ));
            }
            break;
        }
        remaining = remaining.saturating_sub(entry.chars().count());
        spans.push(Span::styled(
            entry,
            Style::default().fg(theme.fg).bg(theme.input_bg),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.input_bg)),
        area,
    );
}
