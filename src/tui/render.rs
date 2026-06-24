use crate::app::AppState;
use crate::app::events::Modal;
use crate::config::Config;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    terminal.draw(|frame| {
        let area = frame.area();
        render_background(frame, area, &theme);

        if state.messages.is_empty() && !state.is_generating {
            render_landing(frame, area, state, config, &theme);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(5),
                    Constraint::Length(1),
                ])
                .split(area);

            widgets::chat::render_chat(frame, chunks[0], state, &theme);
            widgets::input::render_input(frame, chunks[1], state, &theme, false);
            widgets::status::render_status(frame, chunks[2], state, config, &theme);
        }

        if let Some(modal) = &state.modal {
            render_modal(frame, area, modal, &theme);
        }
    })?;
    if state.modal.is_some() || state.is_generating {
        terminal.hide_cursor()?;
    } else {
        terminal.show_cursor()?;
    }
    Ok(())
}

fn render_background(frame: &mut Frame, area: Rect, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(String::new()).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn render_landing(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme) {
    let outer = centered_rect(area, area.width.min(88), area.height.min(22));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.panel));

    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);

    let title = animated_title(state, theme);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(title),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "A DeepSeek-powered coding cockpit for fast local execution",
                    Style::default().fg(theme.text_soft).bg(theme.panel),
                ),
            ]),
        ])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.panel)),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Provider  ", Style::default().fg(theme.bg).bg(theme.accent)),
                Span::styled(
                    format!(" {} · {} ", provider_label(config), config.provider.model),
                    Style::default().fg(theme.fg).bg(theme.panel),
                ),
                Span::styled("  Tools  ", Style::default().fg(theme.bg).bg(theme.success)),
                Span::styled(
                    " Shell + MCP ",
                    Style::default().fg(theme.fg).bg(theme.panel),
                ),
            ]),
        ])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.panel)),
        chunks[1],
    );

    widgets::input::render_input(frame, chunks[2], state, theme, true);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
                Span::styled(" send", Style::default().fg(theme.text_soft)),
                Span::styled("   Ctrl+C / Esc", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
                Span::styled(" quit", Style::default().fg(theme.text_soft)),
                Span::styled("   Ctrl+L", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
                Span::styled(" clear", Style::default().fg(theme.text_soft)),
            ]),
        ])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.panel)),
        chunks[3],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Prompt files", Style::default().fg(theme.text_dim)),
                Span::styled(" live in ", Style::default().fg(theme.text_soft)),
                Span::styled("assets/prompts", Style::default().fg(theme.accent_soft)),
            ]),
        ])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.panel)),
        chunks[4],
    );

    let status_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    widgets::status::render_status(frame, status_area, state, config, theme);
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &Modal, theme: &Theme) {
    let popup_width = area.width.min(60);
    let popup_height = match modal {
        Modal::ToolApproval { .. } => 12,
        Modal::Confirm { .. } => 6,
        Modal::Input { .. } => 6,
    };
    let popup_height = popup_height.min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    match modal {
        Modal::ToolApproval { tool_name, arguments } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .title(" Tool Approval ")
                .title_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.bg));

            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Tool: ",
                        Style::default().fg(theme.text_dim),
                    ),
                    Span::styled(
                        tool_name.clone(),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Arguments:",
                        Style::default().fg(theme.text_dim),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("  {}", arguments.replace('\n', "\n  ")),
                    Style::default().fg(theme.fg),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Allow this tool call? Press Y to allow, N/Esc to deny.",
                        Style::default().fg(theme.fg),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        "  y",
                        Style::default().fg(theme.success).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " = allow  ",
                        Style::default().fg(theme.text_dim),
                    ),
                    Span::styled(
                        "n",
                        Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " = deny",
                        Style::default().fg(theme.text_dim),
                    ),
                ]),
            ];

            frame.render_widget(Clear, popup_area);
            let inner = block.inner(popup_area);
            frame.render_widget(block, popup_area);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg));
            frame.render_widget(paragraph, inner);
        }
        Modal::Confirm { message, .. } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .title(" Confirm ")
                .title_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.bg));

            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        format!("  {message}"),
                        Style::default().fg(theme.fg),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  y",
                        Style::default().fg(theme.success).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " = yes  ",
                        Style::default().fg(theme.text_dim),
                    ),
                    Span::styled(
                        "n",
                        Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " = no",
                        Style::default().fg(theme.text_dim),
                    ),
                ]),
            ];

            frame.render_widget(Clear, popup_area);
            let inner = block.inner(popup_area);
            frame.render_widget(block, popup_area);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg));
            frame.render_widget(paragraph, inner);
        }
        Modal::Input { prompt } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(" Input ")
                .title_style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.bg));

            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        format!("  {prompt}"),
                        Style::default().fg(theme.fg),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Press Esc to cancel",
                        Style::default().fg(theme.text_dim),
                    ),
                ]),
            ];

            frame.render_widget(Clear, popup_area);
            let inner = block.inner(popup_area);
            frame.render_widget(block, popup_area);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg));
            frame.render_widget(paragraph, inner);
        }
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn provider_label(config: &Config) -> &'static str {
    match config.provider.kind {
        crate::config::ProviderKind::Deepseek => "DeepSeek",
        crate::config::ProviderKind::Openai => "OpenAI",
        crate::config::ProviderKind::Custom => "Custom",
    }
}

fn animated_title(state: &AppState, theme: &Theme) -> Vec<Span<'static>> {
    let text = "POOPRUSTEEK";
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let pulse = ((state.animation_tick as usize / 5) + index) % 6;
            let color = match pulse {
                0 | 1 => theme.accent_soft,
                2 | 3 => theme.accent,
                _ => theme.success,
            };
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(color)
                    .bg(theme.panel)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}
