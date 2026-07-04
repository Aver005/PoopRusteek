//! The `/providers` management screen (list of the built-in DeepSeek
//! provider + OpenAI-compatible entries) and the add-wizard modal.

use super::popup::{center_popup, fill_panel_space, modal_block, push_text_box_lines, separator_line};
use crate::app::providers::{provider_rows, ProviderAddState, ProviderWizardStep, ProvidersViewState};
use crate::config::Config;
use crate::tui::theme::Theme;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

pub(super) fn render_providers_view(
    frame: &mut Frame,
    area: Rect,
    view: &ProvidersViewState,
    config: &Config,
    theme: &Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);

    // Header
    let header = Block::default()
        .borders(Borders::ALL)
        .title(" Providers ")
        .border_style(Style::default().fg(theme.accent));
    frame.render_widget(header, chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "LLM backends — the built-in DeepSeek web client and OpenAI-compatible endpoints",
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        chunks[0],
    );

    // Body: one row per provider.
    let rows = provider_rows(config);
    let body = chunks[1];
    for (i, row) in rows.iter().enumerate() {
        if i as u16 >= body.height {
            break;
        }
        let selected = i == view.selected;
        let bg = if selected { theme.accent_dim } else { theme.bg };
        let marker = if row.active { " ● " } else { " ○ " };
        let marker_color = if row.active { theme.success } else { theme.text_dim };
        let tag = if row.builtin { "[built-in]" } else { "[openai-compat]" };

        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(marker_color).bg(bg)),
            Span::styled(
                format!("{:<16} ", row.name),
                if selected {
                    Style::default().fg(theme.bg).bg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg).bg(bg)
                },
            ),
            Span::styled(format!("{tag:<16} "), Style::default().fg(theme.accent_soft).bg(bg)),
            Span::styled(row.detail.clone(), Style::default().fg(theme.text_dim).bg(bg)),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(body.x, body.y + i as u16, body.width, 1),
        );
    }

    // Status message
    if !view.status_message.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                &view.status_message,
                Style::default().fg(theme.success),
            )))
            .alignment(Alignment::Center),
            Rect::new(body.x, body.y + body.height.saturating_sub(1), body.width, 1),
        );
    }

    // Footer with keybindings
    let footer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  j/k ↑↓ navigate  Enter/Space activate  a add  d remove  Esc/q back  ",
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        chunks[2],
    );
    frame.render_widget(footer, chunks[2]);
}

pub(super) fn render_provider_add(
    frame: &mut Frame,
    area: Rect,
    wizard: &ProviderAddState,
    theme: &Theme,
) {
    let popup_width = area.width.clamp(52, 76);
    let popup_h = 14u16.min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_width, popup_h);

    let block = modal_block(" Add Provider ", theme.accent, theme);
    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;
    let max_rows = inner.height.saturating_sub(6) as usize;

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            format!("  Step {}/5 \u{2014} ", wizard.step.number()),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled(
            wizard.step.label(),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    match wizard.step {
        ProviderWizardStep::Name => {
            push_text_box_lines(&mut lines, &wizard.name.buffer, wizard.name.cursor, theme, max_rows)
        }
        ProviderWizardStep::BaseUrl => {
            push_text_box_lines(&mut lines, &wizard.base_url.buffer, wizard.base_url.cursor, theme, max_rows)
        }
        ProviderWizardStep::ApiKey => {
            // Mask the key like the token input — it's a secret on screen.
            let masked = "\u{2022}".repeat(wizard.api_key.buffer.chars().count());
            push_text_box_lines(&mut lines, &masked, wizard.api_key.cursor, theme, max_rows)
        }
        ProviderWizardStep::Model => {
            push_text_box_lines(&mut lines, &wizard.model.buffer, wizard.model.cursor, theme, max_rows)
        }
        ProviderWizardStep::Confirm => {
            lines.push(Line::from(Span::styled(
                format!("  Name:     {}", wizard.name.buffer.trim()),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Base URL: {}", wizard.base_url.buffer.trim()),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  API key:  {}",
                    if wizard.api_key.buffer.trim().is_empty() { "(none)" } else { "(set)" },
                ),
                Style::default().fg(theme.text_soft),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Model:    {}", wizard.model.buffer.trim()),
                Style::default().fg(theme.fg),
            )));
        }
    }

    if let Some(error) = &wizard.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  \u{26A0} {error}"),
            Style::default().fg(theme.error),
        )));
    }

    let hint = match wizard.step {
        ProviderWizardStep::ApiKey => " Enter next (blank = none)    Esc back ",
        ProviderWizardStep::Confirm => " Enter add provider    Esc back ",
        _ => " Enter next    Esc back ",
    };

    fill_panel_space(&mut lines, inner.height.saturating_sub(2) as usize);
    lines.push(separator_line(inner_w, theme));
    lines.push(Line::from(Span::styled(hint, Style::default().fg(theme.text_dim))));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(theme.panel)), inner);
}
