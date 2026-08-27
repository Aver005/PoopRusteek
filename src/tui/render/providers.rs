//! The `/providers` management screen (list of the built-in DeepSeek
//! provider + OpenAI-compatible entries) and the add-wizard modal.

use super::chrome::{panel_footer, panel_frame};
use super::popup::{
    center_popup, draw_popup, fill_panel_space, modal_block, option_row, push_text_box_lines,
    separator_line,
};
use crate::app::list::{ListWindow, NAV_HINT};
use crate::app::providers::{
    ProviderAddState, ProviderWizardStep, ProvidersViewState, provider_rows,
};
use crate::config::Config;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(super) fn render_providers_view(
    frame: &mut Frame,
    area: Rect,
    view: &ProvidersViewState,
    config: &Config,
    theme: &Theme,
) {
    let (body, footer_area) = panel_frame(
        frame,
        area,
        " Providers ",
        "LLM backends — the built-in DeepSeek web client and OpenAI-compatible endpoints",
        theme,
    );

    // Body: one row per provider.
    let rows = provider_rows(config);
    let window = ListWindow::anchored(view.selected, rows.len(), body.height as usize);
    for (slot, i) in window.range().enumerate() {
        let row = &rows[i];
        let selected = i == view.selected;
        let bg = if selected { theme.accent_dim } else { theme.bg };
        let marker = if row.active { " ● " } else { " ○ " };
        let marker_color = if row.active {
            theme.success
        } else {
            theme.text_dim
        };

        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(marker_color).bg(bg)),
            Span::styled(
                format!("{:<16} ", row.name),
                if selected {
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg).bg(bg)
                },
            ),
            Span::styled(
                format!("{:<18} ", format!("[{}]", row.tag)),
                Style::default().fg(theme.accent_soft).bg(bg),
            ),
            Span::styled(
                row.detail.clone(),
                Style::default().fg(theme.text_dim).bg(bg),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(body.x, body.y + slot as u16, body.width, 1),
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
            Rect::new(
                body.x,
                body.y + body.height.saturating_sub(1),
                body.width,
                1,
            ),
        );
    }

    panel_footer(
        frame,
        footer_area,
        format!("  {NAV_HINT} navigate  Enter/Space activate  a add  d remove  Esc/q back  "),
        theme,
    );
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
            format!("  Step {}/6 \u{2014} ", wizard.step.number()),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled(
            wizard.step.label(),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    match wizard.step {
        ProviderWizardStep::Name => push_text_box_lines(
            &mut lines,
            &wizard.name.buffer,
            wizard.name.cursor,
            theme,
            max_rows,
        ),
        ProviderWizardStep::Protocol => {
            for (i, protocol) in crate::app::providers::PROTOCOL_CHOICES.iter().enumerate() {
                lines.push(option_row(
                    crate::app::providers::protocol_choice_label(*protocol),
                    i == wizard.protocol_selected,
                    theme,
                ));
            }
        }
        ProviderWizardStep::BaseUrl => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    crate::app::providers::base_url_example(wizard.protocol())
                ),
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::from(""));
            push_text_box_lines(
                &mut lines,
                &wizard.base_url.buffer,
                wizard.base_url.cursor,
                theme,
                max_rows,
            )
        }
        ProviderWizardStep::ApiKey => {
            // Mask the key like the token input — it's a secret on screen.
            let masked = "\u{2022}".repeat(wizard.api_key.buffer.chars().count());
            push_text_box_lines(&mut lines, &masked, wizard.api_key.cursor, theme, max_rows)
        }
        ProviderWizardStep::Model => push_text_box_lines(
            &mut lines,
            &wizard.model.buffer,
            wizard.model.cursor,
            theme,
            max_rows,
        ),
        ProviderWizardStep::Confirm => {
            lines.push(Line::from(Span::styled(
                format!("  Name:     {}", wizard.name.buffer.trim()),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Protocol: {}", wizard.protocol().label()),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Base URL: {}", wizard.base_url.buffer.trim()),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  API key:  {}",
                    if wizard.api_key.buffer.trim().is_empty() {
                        "(none)"
                    } else {
                        "(set)"
                    },
                ),
                Style::default().fg(theme.text_soft),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  Model:    {}",
                    match wizard.model.buffer.trim() {
                        "" => "(auto — provider's model list)",
                        model => model,
                    },
                ),
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
        ProviderWizardStep::Protocol => " \u{2191}\u{2193} choose    Enter next    Esc back ",
        ProviderWizardStep::ApiKey => " Enter next (blank = none)    Esc back ",
        ProviderWizardStep::Confirm => " Enter add provider    Esc back ",
        _ => " Enter next    Esc back ",
    };

    fill_panel_space(&mut lines, inner.height.saturating_sub(2) as usize);
    lines.push(separator_line(inner_w, theme));
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(theme.text_dim),
    )));

    draw_popup(frame, popup_area, inner, block, lines, theme);
}
