//! MCP-related screens: the full-screen server-management view
//! (`View::Mcp`, list + details) and the `/mcp add` modal.

use super::chrome::{panel_footer, panel_frame};
use super::popup::{
    center_popup, draw_popup, fill_panel_space, modal_block, option_row, push_text_box_lines,
    separator_line,
};
use crate::app::list::{ListWindow, NAV_HINT};
use crate::mcp::types::McpViewState;
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub(super) fn render_mcp_view(frame: &mut Frame, area: Rect, mcp: &McpViewState, theme: &Theme) {
    let (body_area, footer_area) = panel_frame(
        frame,
        area,
        if mcp.auth_mode {
            " MCP Authorization "
        } else {
            " MCP Server Management "
        },
        if mcp.auth_mode {
            "Servers requiring authorization — Enter to authorize"
        } else {
            "Manage your MCP servers — toggle, reconnect, or remove"
        },
        theme,
    );

    if let Some(detail_name) = &mcp.details_server {
        render_mcp_details(frame, body_area, mcp, detail_name, theme);
    } else {
        render_mcp_list(frame, body_area, mcp, theme);
    }

    let hints = if mcp.auth_mode {
        format!("  {NAV_HINT} navigate  Enter authorize  Esc/q back  ")
    } else if mcp.details_server.is_some() {
        "  j/k ↑↓ scroll  Enter back  Esc/q close  ".to_string()
    } else {
        format!(
            "  {NAV_HINT} navigate  Space toggle  r reconnect  d remove  Enter details  Esc/q back  "
        )
    };
    panel_footer(frame, footer_area, hints, theme);
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

    // -1 под строку заголовка.
    let list_height = area.height.saturating_sub(1) as usize;

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

    let window = ListWindow::anchored(mcp.selected, visible_indices.len(), list_height);
    for (i, pos) in window.range().enumerate() {
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
        .scroll((mcp.details_scroll as u16, 0));
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
                lines.push(option_row(opt.to_string(), i == *selected, theme));
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
                        lines.push(option_row(
                            choice.label(),
                            i == wiz.transport_selected,
                            theme,
                        ));
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

    draw_popup(frame, popup_area, inner, block, lines, theme);
}
