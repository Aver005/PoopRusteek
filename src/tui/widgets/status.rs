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

    let status_style = if state.is_generating {
        Style::default()
            .fg(theme.warning)
            .bg(theme.status_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_dim).bg(theme.status_bg)
    };

    let left = format!(" {provider_name} · {model} ");
    let center = state.status_message.clone();
    let right = format!(" msgs:{msg_count} ");

    let status_line = Line::from(vec![
        Span::styled(&left, status_style),
        Span::styled(
            " ".repeat(area.width.saturating_sub(
                (left.len() + center.len() + right.len()) as u16
            ).max(0) as usize),
            Style::default().fg(theme.text_dim).bg(theme.status_bg),
        ),
        Span::styled(&center, status_style),
        Span::styled(" ", Style::default().bg(theme.status_bg)),
        Span::styled(&right, status_style),
    ]);

    let paragraph = Paragraph::new(status_line).style(Style::default().bg(theme.status_bg));
    frame.render_widget(paragraph, area);
}
