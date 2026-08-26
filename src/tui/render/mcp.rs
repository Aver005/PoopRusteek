//! MCP-related screens: the full-screen server-management view
//! (`View::Mcp`, list + details) and the `/mcp add` modal.

use super::popup::{
    center_popup, fill_panel_space, modal_block, push_text_box_lines, separator_line,
};
use crate::mcp::types::McpViewState;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub(super) fn render_mcp_view(frame: &mut Frame, area: Rect, mcp: &McpViewState, theme: &Theme) {
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
        .title(if mcp.auth_mode {
            " MCP Authorization "
        } else {
            " MCP Server Management "
        })
        .border_style(Style::default().fg(theme.accent));
    let header_inner = header.inner(chunks[0]);
    frame.render_widget(header, chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if mcp.auth_mode {
                "Servers requiring authorization — Enter to authorize"
            } else {
                "Manage your MCP servers — toggle, reconnect, or remove"
            },
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        header_inner,
    );

    // Body
    let body_area = chunks[1];
    if let Some(detail_name) = &mcp.details_server {
        render_mcp_details(frame, body_area, mcp, detail_name, theme);
    } else {
        render_mcp_list(frame, body_area, mcp, theme);
    }

    // Footer with keybindings. Block first, hints into its inner row — the
    // old order painted the hint text onto the border row and the block's
    // top border then overdrew it, leaving the legend invisible.
    let footer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let hints = if mcp.auth_mode {
        "  j/k ↑↓ navigate  Enter authorize  Esc/q back  "
    } else if mcp.details_server.is_some() {
        "  j/k ↑↓ scroll  Enter back  Esc/q close  "
    } else {
        "  j/k ↑↓ navigate  Space toggle  r reconnect  d remove  Enter details  Esc/q back  "
    };
    let footer_inner = footer.inner(chunks[2]);
    frame.render_widget(footer, chunks[2]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints,
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        footer_inner,
    );
}

fn render_mcp_list(frame: &mut Frame, area: Rect, mcp: &McpViewState, theme: &Theme) {
    // Identity (`0..servers.len()`) outside `auth_mode`; filtered to
    // `needs_auth` servers inside it — see `McpViewState::visible_indices`.
    let visible_indices = mcp.visible_indices();

    if mcp.auth_mode && visible_indices.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No servers currently require authorization",
                Style::default().fg(theme.text_dim),
            )))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let list_height = area.height.saturating_sub(1) as usize;
    let visible = visible_indices.len().min(list_height);

    let header_line = Line::from(vec![
        Span::styled(
            "  STATUS  ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "SERVER",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(
            " ".repeat(area.width.saturating_sub(30) as usize)
                .min(" ".repeat(20)),
        ),
        Span::styled(
            "TYPE   TOOLS",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(header_line), area);

    let start = mcp.scroll_offset;
    for i in 0..visible {
        let pos = start + i;
        if pos >= visible_indices.len() {
            break;
        }
        let idx = visible_indices[pos];
        let server = &mcp.servers[idx];
        let selected = pos == mcp.selected;
        let bg = if selected { theme.accent_dim } else { theme.bg };

        let status_icon = match server.status.as_str() {
            "disabled" => " ○ ",
            _ if server.status.starts_with("error") => " ✗ ",
            "connected" => " ● ",
            "auth required" => " ⚿ ",
            "pending" | "connecting" => " ◌ ",
            _ => " ? ",
        };
        let status_color = match server.status.as_str() {
            "disabled" => theme.text_dim,
            _ if server.status.starts_with("error") => theme.error,
            "connected" => theme.success,
            "auth required" => theme.warning,
            "pending" | "connecting" => theme.warning,
            _ => theme.fg,
        };
        let status_short = match server.status.as_str() {
            s if s.starts_with("error") => "ERR ",
            "disabled" => "OFF ",
            "connected" => "ON  ",
            "auth required" => "AUTH",
            "pending" => "WAIT",
            "connecting" => "CONN",
            _ => "?   ",
        };
        let dim_style = Style::default().fg(theme.text_dim).bg(bg);

        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", status_short),
                Style::default()
                    .fg(status_color)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {} ", status_icon), dim_style),
            Span::styled(
                format!("{:<20} ", server.name),
                if selected {
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg).bg(bg)
                },
            ),
            Span::styled(format!("{:<6}", server.transport), dim_style),
            Span::styled(
                format!("{}t", server.tool_count),
                Style::default().fg(theme.fg).bg(bg),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + 1 + i as u16, area.width, 1),
        );
    }

    // Status message
    if !mcp.status_message.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                &mcp.status_message,
                Style::default().fg(theme.success),
            )))
            .alignment(Alignment::Center),
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        );
    }
}

fn render_mcp_details(
    frame: &mut Frame,
    area: Rect,
    mcp: &McpViewState,
    server_name: &str,
    theme: &Theme,
) {
    let Some(server) = mcp.servers.iter().find(|s| s.name == server_name) else {
        return;
    };

    let lines: Vec<Line> = {
        let mut out = Vec::new();
        out.push(Line::from(Span::styled(
            format!(" Server: {}", server.name),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        out.push(Line::from(Span::styled(
            format!(
                " Status: {} ({})",
                server.status,
                if server.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            Style::default().fg(theme.fg),
        )));
        out.push(Line::from(Span::styled(
            format!(" Transport: {}", server.transport),
            Style::default().fg(theme.text_dim),
        )));
        out.push(Line::from(Span::styled(
            format!(" Tools: {}", server.tool_count),
            Style::default().fg(theme.fg),
        )));
        out.push(Line::from(Span::styled(
            format!(" Resources: {}", server.resource_count),
            Style::default().fg(theme.fg),
        )));
        out.push(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(theme.border),
        )));
        out.push(Line::from(Span::styled(
            " j/k scroll  Enter back ",
            Style::default().fg(theme.text_dim),
        )));
        out
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", server_name))
        .border_style(Style::default().fg(theme.accent));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((mcp.scroll_offset as u16, 0));
    frame.render_widget(paragraph, area);
}

pub(super) fn render_mcp_add(
    frame: &mut Frame,
    area: Rect,
    state: &crate::app::mcp_add::McpAddState,
    theme: &Theme,
) {
    use crate::app::mcp_add::{McpAddState, TransportChoice, WizardStep};

    let popup_width = area.width.clamp(52, 76);
    let popup_h = 16u16.min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_width, popup_h);

    let block = modal_block(" Add MCP Server ", theme.accent, theme);
    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;
    let max_rows = inner.height.saturating_sub(6) as usize;

    let mut lines: Vec<Line> = Vec::new();

    let hint: &str = match state {
        McpAddState::ChooseMethod { selected } => {
            lines.push(Line::from(Span::styled(
                "  Pick a method:",
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::from(""));
            for (i, opt) in ["Paste a JSON config", "Step-by-step wizard"]
                .iter()
                .enumerate()
            {
                let is_cursor = i == *selected;
                let indicator = if is_cursor { "\u{25B6} " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.fg)),
                    Span::styled(
                        indicator,
                        Style::default().fg(if is_cursor {
                            theme.accent
                        } else {
                            theme.text_dim
                        }),
                    ),
                    Span::styled(
                        opt.to_string(),
                        if is_cursor {
                            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.text_soft)
                        },
                    ),
                ]));
            }
            " \u{2191}\u{2193} navigate    Enter select    Esc cancel "
        }
        McpAddState::PasteJson { input, error } => {
            lines.push(Line::from(Span::styled(
                "  Paste a server config JSON (single object, or {\"mcpServers\": {...}}):",
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::from(""));
            push_text_box_lines(&mut lines, &input.buffer, input.cursor, theme, max_rows);
            if let Some(err) = error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  \u{26A0} {err}"),
                    Style::default().fg(theme.error),
                )));
            }
            " Enter add    Shift+Enter newline    Esc back "
        }
        McpAddState::Wizard(wiz) => {
            let (step_num, step_label) = match wiz.step {
                WizardStep::Name => (1, "Server name".to_string()),
                WizardStep::Transport => (2, "Transport".to_string()),
                WizardStep::Primary => (
                    3,
                    wiz.transport
                        .map(TransportChoice::primary_label)
                        .unwrap_or("Value")
                        .to_string(),
                ),
                WizardStep::Extra => (
                    4,
                    wiz.transport
                        .map(TransportChoice::extra_label)
                        .unwrap_or("Extra")
                        .to_string(),
                ),
                WizardStep::Confirm => (5, "Confirm".to_string()),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  Step {step_num}/5 \u{2014} "),
                    Style::default().fg(theme.text_dim),
                ),
                Span::styled(
                    step_label,
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(""));

            match wiz.step {
                WizardStep::Name => push_text_box_lines(
                    &mut lines,
                    &wiz.name.buffer,
                    wiz.name.cursor,
                    theme,
                    max_rows,
                ),
                WizardStep::Transport => {
                    for (i, choice) in TransportChoice::ALL.iter().enumerate() {
                        let is_cursor = i == wiz.transport_selected;
                        let indicator = if is_cursor { "\u{25B6} " } else { "  " };
                        lines.push(Line::from(vec![
                            Span::styled("  ", Style::default().fg(theme.fg)),
                            Span::styled(
                                indicator,
                                Style::default().fg(if is_cursor {
                                    theme.accent
                                } else {
                                    theme.text_dim
                                }),
                            ),
                            Span::styled(
                                choice.label(),
                                if is_cursor {
                                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(theme.text_soft)
                                },
                            ),
                        ]));
                    }
                }
                WizardStep::Primary => push_text_box_lines(
                    &mut lines,
                    &wiz.primary.buffer,
                    wiz.primary.cursor,
                    theme,
                    max_rows,
                ),
                WizardStep::Extra => push_text_box_lines(
                    &mut lines,
                    &wiz.extra.buffer,
                    wiz.extra.cursor,
                    theme,
                    max_rows,
                ),
                WizardStep::Confirm => {
                    lines.push(Line::from(Span::styled(
                        format!("  Name:      {}", wiz.name.buffer.trim()),
                        Style::default().fg(theme.fg),
                    )));
                    if let Some(t) = wiz.transport {
                        lines.push(Line::from(Span::styled(
                            format!("  Transport: {}", t.label()),
                            Style::default().fg(theme.fg),
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        format!("  {}", wiz.primary.buffer.trim()),
                        Style::default().fg(theme.text_soft),
                    )));
                    if !wiz.extra.buffer.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            "  (+ extra env/headers)",
                            Style::default().fg(theme.text_dim),
                        )));
                    }
                }
            }

            if let Some(err) = &wiz.error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  \u{26A0} {err}"),
                    Style::default().fg(theme.error),
                )));
            }

            match wiz.step {
                WizardStep::Name | WizardStep::Primary => " Enter next    Esc back ",
                WizardStep::Extra => {
                    " Enter next (blank = skip)    Shift+Enter newline    Esc back "
                }
                WizardStep::Transport => " \u{2191}\u{2193} navigate    Enter select    Esc back ",
                WizardStep::Confirm => " Enter add server    Esc back ",
            }
        }
    };

    fill_panel_space(&mut lines, inner.height.saturating_sub(2) as usize);
    lines.push(separator_line(inner_w, theme));
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(theme.text_dim),
    )));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}
