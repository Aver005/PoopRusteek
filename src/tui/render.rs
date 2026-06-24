use crate::app::AppState;
use crate::app::events::Modal;
use crate::config::Config;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    terminal.draw(|frame| {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // Chat area
                Constraint::Length(3), // Input area
                Constraint::Length(1), // Status bar
            ])
            .split(area);

        // Chat messages
        widgets::chat::render_chat(frame, chunks[0], state, &theme);

        // Input box
        widgets::input::render_input(frame, chunks[1], state, &theme);

        // Status bar
        widgets::status::render_status(frame, chunks[2], state, config, &theme);

        // Modal overlay
        if let Some(modal) = &state.modal {
            render_modal(frame, area, modal, &theme);
        }
    })?;
    Ok(())
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &Modal, theme: &Theme) {
    let popup_width = area.width.min(60);
    let popup_height = match modal {
        Modal::ToolApproval { .. } => 8,
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
                        "  Args: ",
                        Style::default().fg(theme.text_dim),
                    ),
                    Span::styled(
                        arguments.clone(),
                        Style::default().fg(theme.fg),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Allow this tool call?",
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

            let inner = block.inner(inner_area(area, popup_area));
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

            let inner = block.inner(popup_area);
            frame.render_widget(block, popup_area);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg));
            frame.render_widget(paragraph, inner);
        }
    }
}

fn inner_area(_area: Rect, popup: Rect) -> Rect {
    popup
}
