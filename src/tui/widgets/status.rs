use crate::app::AppState;
use crate::config::Config;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub fn render_status(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme) {
    let provider_name = match config.provider.kind {
        crate::config::ProviderKind::Deepseek => "DeepSeek",
        crate::config::ProviderKind::Openai => "OpenAI",
        crate::config::ProviderKind::Custom => "Custom",
    };
    let model = &config.provider.model;
    let msg_count = state.messages.len();
    let total_tokens: u32 = state.messages.iter()
        .filter(|m| m.role == crate::provider::Role::Assistant)
        .flat_map(|m| m.total_tokens)
        .sum();

    let mcp_status = if state.mcp_server_count > 0 {
        format!(" mcp:{}/{}", state.mcp_server_connected_count, state.mcp_server_count)
    } else {
        String::new()
    };

    let model_tag = if !state.last_model_name.is_empty() {
        format!(" · {}", state.last_model_name)
    } else {
        String::new()
    };
    let left = format!(" {} · {}{}{} ", provider_name, model, model_tag, mcp_status);

    let status_tag = state.last_message_status.as_deref().unwrap_or("");

    let center = if state.is_generating {
        let gen_info = if let Some(start) = state.generation_start_time {
            let elapsed = start.elapsed().as_secs_f64();
            let tps = if elapsed > 0.0 && state.last_gen_tokens > 0 {
                state.last_gen_tokens as f64 / elapsed
            } else {
                0.0
            };
            format!(" {} {} ({} tok, {:.1}s, {:.0} t/s)", spinner_char(state.animation_tick), state.status_message, state.last_gen_tokens, elapsed, tps)
        } else {
            format!(" {} {}", spinner_char(state.animation_tick), state.status_message)
        };
        gen_info
    } else {
        let mut parts = vec![state.status_message.clone()];
        if !status_tag.is_empty() {
            parts.push(status_tag.to_string());
        }
        if state.last_think_fragments > 0 {
            parts.push(format!("{} think", state.last_think_fragments));
        }
        if state.last_gen_duration_secs > 0.0 && state.last_gen_tokens > 0 {
            let tps = state.last_gen_tokens as f64 / state.last_gen_duration_secs;
            parts.push(format!("{} tok in {:.1}s ({:.0} t/s)", state.last_gen_tokens, state.last_gen_duration_secs, tps));
        }
        format!(" {} ", parts.join(" · "))
    };

    let session_prefix = if state.current_session_id.len() >= 8 {
        &state.current_session_id[..8]
    } else {
        &state.current_session_id
    };
    let right = format!(" msgs:{} tot:{} | {} ", msg_count, total_tokens, session_prefix);

    let status_style = if state.is_generating {
        Style::default()
            .fg(theme.warning)
            .bg(theme.status_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_dim).bg(theme.status_bg)
    };

    let gap = area.width.saturating_sub((left.len() + center.len() + right.len()) as u16).max(1) as usize;

    let status_line = Line::from(vec![
        Span::styled(&left, status_style),
        Span::styled(
            " ".repeat(gap),
            Style::default().fg(theme.text_dim).bg(theme.status_bg),
        ),
        Span::styled(&center, status_style),
        Span::styled(" ", Style::default().bg(theme.status_bg)),
        Span::styled(&right, status_style),
    ]);

    let paragraph = Paragraph::new(status_line).style(Style::default().bg(theme.status_bg));
    frame.render_widget(paragraph, area);
}

fn spinner_char(tick: u64) -> &'static str {
    match tick % 4 {
        0 => "|",
        1 => "/",
        2 => "-",
        _ => "\\",
    }
}
