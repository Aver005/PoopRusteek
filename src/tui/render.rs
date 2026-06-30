use crate::app::AppState;
use crate::app::events::{Modal, PickerMode, PickerState, QuestionState, View};
use crate::config::Config;
use crate::mcp::types::McpViewState;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::view_model;
use crate::tui::widgets;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::cell::Cell;

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    let cursor_cell = Cell::new(None);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(String::new()).style(Style::default().bg(theme.bg)),
            area,
        );

        if state.view == View::Mcp {
            render_mcp_view(frame, area, &state.mcp_status.view, &theme);
        } else if state.messages.is_empty() && !state.generation.active {
            render_landing(frame, area, state, config, &theme, &cursor_cell);
        } else {
            let has_attachments = !state.attached_files.is_empty();
            let mut constraints = vec![
                Constraint::Min(1),
                Constraint::Length(1),
            ];
            if has_attachments {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Length(4));
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(1));

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            render_separator(frame, chunks[1], &theme);

            // Split main content into chat + stats panel
            let pad_w = 2u16;
            if state.show_stats_panel {
                let panel_w = 34u16.min(chunks[0].width / 4);
                let content_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(pad_w),
                        Constraint::Min(1),
                        Constraint::Length(panel_w),
                    ])
                    .split(chunks[0]);
                widgets::chat::render_chat(frame, content_chunks[1], state, &theme);
                widgets::panel::render_stats_panel(frame, chunks[0], state, config, &theme);
            } else {
                let content_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(pad_w),
                        Constraint::Min(1),
                        Constraint::Length(pad_w),
                    ])
                    .split(chunks[0]);
                widgets::chat::render_chat(frame, content_chunks[1], state, &theme);
            }

            let attach_idx = 2usize;
            let input_idx = if has_attachments { 3usize } else { 2usize };
            let border_idx = if has_attachments { 4usize } else { 3usize };
            let status_idx = if has_attachments { 5usize } else { 4usize };

            if has_attachments {
                render_attach_bar(frame, chunks[attach_idx], state, &theme);
            }
            widgets::input::render_input(frame, chunks[input_idx], state, &theme, false, &cursor_cell);
            render_input_border(frame, chunks[border_idx], &theme);
            render_mini_status(frame, chunks[status_idx], state, config, &theme);

            if state.autocomplete.visible {
                render_autocomplete(frame, chunks[2], state, &theme);
            }
        }

        if let Some(modal) = &state.modal {
            render_modal(frame, area, modal, &theme);
        }
    })?;
    // Re-set cursor position after frame flush to prevent flicker onto (0,0)
    if let Some((cx, cy)) = cursor_cell.get() {
        use crossterm::cursor::MoveTo;
        crossterm::execute!(terminal.backend_mut(), MoveTo(cx, cy))?;
    }
    if state.modal.is_some() || state.generation.active || state.view == View::Mcp {
        terminal.hide_cursor()?;
    } else {
        terminal.show_cursor()?;
    }
    Ok(())
}

fn render_separator(frame: &mut Frame, area: Rect, theme: &Theme) {
    let line = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            line,
            Style::default().fg(theme.border).bg(theme.bg),
        ))),
        area,
    );
}

fn render_input_border(frame: &mut Frame, area: Rect, theme: &Theme) {
    let w = area.width.saturating_sub(3) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}{}", "   ", "─".repeat(w)),
            Style::default().fg(theme.border).bg(theme.input_bg),
        ))),
        area,
    );
}

fn render_landing(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme, cursor_cell: &Cell<Option<(u16, u16)>>) {
    let input_width = area.width.min(76);
    let x = (area.width - input_width) / 2;

    let sessions = crate::session::list_sessions(config)
        .unwrap_or_default();
    let session_count = sessions.len();
    let show_sessions = session_count > 0;

    // Logo(3) + gap(1) + input(4) + border(1) + shortcuts(1) + gap(1) + sessions(if any: 1+count.min(5)+1) + gap + status(1)
    let center_h = 3 + 1 + 4 + 1 + 1 + 1
        + if show_sessions { 1 + sessions.len().min(5) as u16 + 1 } else { 0 };
    let top_pad = (area.height.saturating_sub(center_h)) / 2;

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(top_pad),
        Constraint::Length(3),   // logo block
        Constraint::Length(1),   // gap
        Constraint::Length(4),   // input
        Constraint::Length(1),   // border
        Constraint::Length(1),   // shortcuts
        Constraint::Length(1),   // gap
    ];
    if show_sessions {
        let count = sessions.len().min(5) as u16;
        constraints.push(Constraint::Length(count + 2)); // header + rows + blank
    }
    constraints.push(Constraint::Min(0)); // bottom space

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut ci = 1;

    // Logo block
    let logo_area = centered_h(chunks[ci], input_width);
    ci += 1;
    let title_spans = bigger_title(state, theme);
    frame.render_widget(
        Paragraph::new(Line::from(title_spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(theme.bg)),
        logo_area,
    );

    // skip gap (ci points to gap, which we skip by incrementing)
    ci += 1;

    // Input — 4 lines, centered
    let inp_area = Rect::new(x, chunks[ci].y, input_width, 4);
    ci += 1;
    widgets::input::render_input(frame, inp_area, state, theme, true, cursor_cell);

    if state.autocomplete.visible {
        render_autocomplete(frame, inp_area, state, theme);
    }

    // Border below input
    let border_area = centered_h(chunks[ci], input_width);
    ci += 1;
    render_input_border(frame, border_area, &theme);

    // Shortcuts line
    let info_area = centered_h(chunks[ci], input_width);
    ci += 1;
    let model = format!("{} · {}", provider_label(config), config.provider.model);
    let mut shortcut_spans = vec![
        Span::styled(model, Style::default().fg(theme.text_soft).bg(theme.bg)),
        Span::styled("  ", Style::default().bg(theme.bg)),
        Span::styled("Enter ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
        Span::styled("send  ", Style::default().fg(theme.text_dim)),
        Span::styled("/ ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
        Span::styled("commands  ", Style::default().fg(theme.text_dim)),
        Span::styled("Esc ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
        Span::styled("home  ", Style::default().fg(theme.text_dim)),
        Span::styled("Ctrl+C ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
        Span::styled("quit", Style::default().fg(theme.text_dim)),
    ];
    if state.goal.mode {
        let goal_label = match state.goal.stage {
            crate::app::events::GoalStage::Inactive => "[GOAL ON]".to_string(),
            crate::app::events::GoalStage::WaitForGoal => "[WAITING FOR GOAL]".to_string(),
            crate::app::events::GoalStage::RunAgent1 => format!("[GOAL iter#{}]", state.goal.iteration),
            crate::app::events::GoalStage::RunEvaluator => "[EVALUATING]".to_string(),
            crate::app::events::GoalStage::Done => "[GOAL DONE]".to_string(),
        };
        shortcut_spans.push(Span::styled("  ", Style::default().bg(theme.bg)));
        shortcut_spans.push(Span::styled(
            goal_label,
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![Line::from(shortcut_spans)])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg)),
        info_area,
    );

    // skip gap
    ci += 1;

    // Sessions table
    if show_sessions {
        let sc = chunks[ci];
        render_sessions_table(frame, sc, &sessions, input_width, theme);
    }

    // Status at very bottom
    let status_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    render_mini_status(frame, status_area, state, config, theme);
}

fn render_sessions_table(
    frame: &mut Frame,
    area: Rect,
    sessions: &[crate::session::SessionSummary],
    input_width: u16,
    theme: &Theme,
) {
    let _count = sessions.len().min(5);
    let area = centered_h(area, input_width);
    let header_style = Style::default().fg(theme.text_dim).bg(theme.bg);
    let id_style = Style::default().fg(theme.accent_soft).bg(theme.bg);
    let title_style = Style::default().fg(theme.fg).bg(theme.bg);
    let date_style = Style::default().fg(theme.text_dim).bg(theme.bg);
    let model_style = Style::default().fg(theme.accent_soft).bg(theme.bg);
    let sep_style = Style::default().fg(theme.border).bg(theme.bg);

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled(" Recent sessions ", header_style),
    ]));
    lines.push(Line::from(vec![Span::styled(
        format!("  {} {} {}  {}", "──".repeat(8), "──".repeat(15), "──".repeat(8), "──".repeat(6)),
        sep_style,
    )]));

    for s in sessions.iter().take(5) {
        let id_short = truncate(&s.id, 16);
        let title = truncate(&s.title, 30);
        let date = format_date(&s.updated_at);
        let model_tag = if s.model_type.is_empty() { String::new() } else { format!(" [{}]", s.model_type) };
        lines.push(Line::from(vec![
            Span::styled(format!("  {id_short}"), id_style),
            Span::styled("  ", Style::default().bg(theme.bg)),
            Span::styled(title.to_string(), title_style),
            Span::styled("  ", Style::default().bg(theme.bg)),
            Span::styled(model_tag, model_style),
            Span::styled("  ", Style::default().bg(theme.bg)),
            Span::styled(date, date_style),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

fn format_date(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        dt.format("%b %d %H:%M").to_string()
    } else {
        rfc3339.chars().take(16).collect()
    }
}

fn centered_h(area: Rect, width: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    Rect::new(x, area.y, width.min(area.width), area.height)
}

fn render_mini_status(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme) {
    let provider_name = provider_label(config);
    let model = &config.provider.model;
    let msg_count = state.messages.len();
    let total_tokens = view_model::assistant_token_total(&state.messages);

    let mcp_status = view_model::mcp_label(&state.mcp_status);
    let bg_status = if state.running_background_count > 0 {
        let mut suffix = String::new();
        if state.running_interactive_count > 0 {
            suffix.push_str(&format!("/{}i", state.running_interactive_count));
        }
        if state.running_persistent_count > 0 {
            suffix.push_str(&format!("/{}p", state.running_persistent_count));
        }
        format!(" bg:{}{}", state.running_background_count, suffix)
    } else {
        String::new()
    };
    let btw_status = {
        let running = state
            .background
            .iter()
            .filter(|c| c.kind == crate::app::conversation::ConversationKind::Sidechat && c.is_streaming())
            .count();
        if running > 0 {
            format!(" btw:{running}")
        } else {
            String::new()
        }
    };
    let chats_status = {
        // Switchable parallel chats = focused + background sessions (not sidechats).
        let sessions = 1 + state
            .background
            .iter()
            .filter(|c| c.kind != crate::app::conversation::ConversationKind::Sidechat)
            .count();
        if sessions > 1 {
            format!(" chats:{sessions}")
        } else {
            String::new()
        }
    };

    let model_tag = if !state.generation.last_model.is_empty() {
        format!(" · {}", state.generation.last_model)
    } else {
        String::new()
    };
    let goal_tag = if state.goal.mode {
        format!(" GOAL:{} ", match state.goal.stage {
            crate::app::events::GoalStage::Inactive => "standby",
            crate::app::events::GoalStage::WaitForGoal => "need-goal",
            crate::app::events::GoalStage::RunAgent1 => "build",
            crate::app::events::GoalStage::RunEvaluator => "eval",
            crate::app::events::GoalStage::Done => "done",
        })
    } else {
        String::new()
    };
    let left = format!(" {}{} · {}{}{}{}{}{} ", goal_tag, provider_name, model, model_tag, mcp_status, bg_status, btw_status, chats_status);

    let status_tag = state.generation.last_status.as_deref().unwrap_or("");

    let center = if state.generation.active {
        let gen_info = if let Some(start) = state.generation.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            let tps = view_model::tokens_per_sec(state.generation.last_tokens, elapsed);
            format!(" {} {} ", view_model::spinner_char(state.generation.animation_tick), state.status_message)
                + &format!("({} tok, {:.1}s, {:.0} t/s)", state.generation.last_tokens, elapsed, tps)
        } else {
            format!(" {} {} ", view_model::spinner_char(state.generation.animation_tick), state.status_message)
        };
        gen_info
    } else {
        let mut parts = vec![state.status_message.clone()];
        if !status_tag.is_empty() {
            parts.push(status_tag.to_string());
        }
        if state.generation.last_think_fragments > 0 {
            parts.push(format!("{} think", state.generation.last_think_fragments));
        }
        let tps = view_model::tokens_per_sec(state.generation.last_tokens, state.generation.last_duration_secs);
        if tps > 0.0 {
            parts.push(format!("{} tok in {:.1}s ({:.0} t/s)", state.generation.last_tokens, state.generation.last_duration_secs, tps));
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
    let session_prefix = format!(" {}", view_model::session_prefix(&state.current_session_id));
    let right = format!("{} msgs:{} tot:{} | {} ", goal_tag, msg_count, total_tokens, session_prefix);

    let gap = area.width.saturating_sub((left.len() + center.len() + right.len()) as u16).max(1) as usize;

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(theme.text_dim).bg(theme.bg)),
            Span::styled(" ".repeat(gap), Style::default().bg(theme.bg)),
            Span::styled(
                center,
                if state.generation.active {
                    Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                }
                .bg(theme.bg),
            ),
            Span::styled(
                right,
                Style::default().fg(theme.text_dim).bg(theme.bg),
            ),
        ])),
        area,
    );
}


fn render_mcp_view(frame: &mut Frame, area: Rect, mcp: &McpViewState, theme: &Theme) {
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
        .title(" MCP Server Management ")
        .border_style(Style::default().fg(theme.accent));
    frame.render_widget(header, chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Manage your MCP servers — toggle, reconnect, or remove",
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        chunks[0],
    );

    // Body
    let body_area = chunks[1];
    if let Some(detail_name) = &mcp.details_server {
        render_mcp_details(frame, body_area, mcp, detail_name, theme);
    } else {
        render_mcp_list(frame, body_area, mcp, theme);
    }

    // Footer with keybindings
    let footer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let hints = if mcp.details_server.is_some() {
        "  j/k ↑↓ scroll  Enter back  Esc/q close  "
    } else {
        "  j/k ↑↓ navigate  Space toggle  r reconnect  d remove  Enter details  Esc/q back  "
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, Style::default().fg(theme.text_dim))))
            .alignment(Alignment::Center),
        chunks[2],
    );
    frame.render_widget(footer, chunks[2]);
}

fn render_mcp_list(frame: &mut Frame, area: Rect, mcp: &McpViewState, theme: &Theme) {
    let list_height = area.height.saturating_sub(1) as usize;
    let visible = mcp.servers.len().min(list_height);

    let header_line = Line::from(vec![
        Span::styled("  STATUS  ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("SERVER", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(area.width.saturating_sub(30) as usize).min(
            " ".repeat(20).to_string(),
        )),
        Span::styled("TYPE   TOOLS", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(header_line), area);

    let start = mcp.scroll_offset;
    for i in 0..visible {
        let idx = start + i;
        if idx >= mcp.servers.len() {
            break;
        }
        let server = &mcp.servers[idx];
        let selected = idx == mcp.selected;
        let bg = if selected { theme.accent_dim } else { theme.bg };

        let status_icon = match server.status.as_str() {
            "disabled" => " ○ ",
            _ if server.status.starts_with("error") => " ✗ ",
            "connected" => " ● ",
            "pending" | "connecting" => " ◌ ",
            _ => " ? ",
        };
        let status_color = match server.status.as_str() {
            "disabled" => theme.text_dim,
            _ if server.status.starts_with("error") => theme.error,
            "connected" => theme.success,
            "pending" | "connecting" => theme.warning,
            _ => theme.fg,
        };
        let status_short = match server.status.as_str() {
            s if s.starts_with("error") => "ERR ",
            "disabled" => "OFF ",
            "connected" => "ON  ",
            "pending" => "WAIT",
            "connecting" => "CONN",
            _ => "?   ",
        };
        let dim_style = Style::default().fg(theme.text_dim).bg(bg);

        let line = Line::from(vec![
            Span::styled(format!(" {} ", status_short), Style::default().fg(status_color).bg(bg).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {} ", status_icon), dim_style),
            Span::styled(
                format!("{:<20} ", server.name),
                if selected {
                    Style::default().fg(theme.bg).bg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg).bg(bg)
                },
            ),
            Span::styled(
                format!("{:<6}", server.transport),
                dim_style,
            ),
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

fn render_mcp_details(frame: &mut Frame, area: Rect, mcp: &McpViewState, server_name: &str, theme: &Theme) {
    let Some(server) = mcp.servers.iter().find(|s| s.name == server_name) else {
        return;
    };

    let lines: Vec<Line> = {
        let mut out = Vec::new();
        out.push(Line::from(Span::styled(
            format!(" Server: {}", server.name),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));
        out.push(Line::from(Span::styled(
            format!(" Status: {} ({})", server.status, if server.enabled { "enabled" } else { "disabled" }),
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
        out.push(Line::from(Span::styled("─".repeat(area.width as usize), Style::default().fg(theme.border))));
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

    // Prevent scroll beyond content
    // (handled in the keyboard handler)
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &Modal, theme: &Theme) {
    match modal {
        Modal::ToolApproval { tool_name, arguments, scroll_offset, always_allow } => {
            let popup_width = area.width.clamp(50, 72);
            let max_height = area.height.saturating_sub(4);
            let content_height = arguments.lines().count().max(4).min(max_height.saturating_sub(6) as usize);
            let popup_height = (content_height + 8).min(max_height as usize) as u16;

            let x = (area.width.saturating_sub(popup_width)) / 2;
            let y = (area.height.saturating_sub(popup_height)) / 2;
            let popup_area = Rect::new(x, y, popup_width, popup_height);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .title(" \u{26A0} Tool Call ")
                .title_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.panel));

            let inner = block.inner(popup_area);
            let inner_w = inner.width as usize;
            let inner_h = inner.height as usize;

            let mut all_lines: Vec<Line> = Vec::new();

            // Tool name line
            all_lines.push(Line::from(vec![
                Span::styled("  \u{2692} ", Style::default().fg(theme.warning)),
                Span::styled("Tool:", Style::default().fg(theme.text_dim)),
                Span::styled(" ", Style::default().fg(theme.fg)),
                Span::styled(
                    tool_name.clone(),
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
            ]));
            all_lines.push(Line::from(""));

            // Arguments header
            all_lines.push(Line::from(vec![
                Span::styled("  \u{1F4CB} Arguments:", Style::default().fg(theme.text_dim)),
            ]));

            // Parse and format arguments with JSON highlighting
            let arg_lines = highlight_json(arguments, theme);
            let max_visible = inner_h.saturating_sub(6);
            let total_args = arg_lines.len();
            let scroll = *scroll_offset;
            let display_lines: Vec<&Line> = if total_args > max_visible {
                let end = (scroll + max_visible).min(total_args);
                arg_lines[scroll..end].iter().collect()
            } else {
                arg_lines.iter().collect()
            };

            for line in &display_lines {
                let mut line_spans = vec![Span::styled("  ", Style::default().fg(theme.fg))];
                for span in &line.spans {
                    line_spans.push(Span::styled(
                        span.content.clone(),
                        span.style,
                    ));
                }
                all_lines.push(Line::from(line_spans));
            }

            // Scroll indicator
            if total_args > max_visible {
                let pct = if total_args > 0 {
                    (scroll as f64 / (total_args.saturating_sub(max_visible)) as f64 * 100.0) as u16
                } else {
                    0
                };
                let indicator = format!("\u{2191}\u{2193} {:.0}%", pct.min(100));
                all_lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", indicator),
                        Style::default().fg(theme.text_dim),
                    ),
                ]));
            }

            // Fill remaining space
            while all_lines.len() < inner_h.saturating_sub(3) {
                all_lines.push(Line::from(""));
            }

            // Separator
            let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
            all_lines.push(Line::from(vec![
                Span::styled(format!("  {}", sep), Style::default().fg(theme.border)),
            ]));

            // Always allow checkbox
            let check = if *always_allow { "\u{2611}" } else { "\u{2610}" };
            all_lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} Always allow for this session", check),
                    if *always_allow {
                        Style::default().fg(theme.success)
                    } else {
                        Style::default().fg(theme.text_dim)
                    },
                ),
            ]));

            // Key bindings
            all_lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.fg)),
                Span::styled("Y", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
                Span::styled(" allow  ", Style::default().fg(theme.text_dim)),
                Span::styled("N", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
                Span::styled(" deny  ", Style::default().fg(theme.text_dim)),
                Span::styled("A", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                Span::styled(" toggle  ", Style::default().fg(theme.text_dim)),
                Span::styled("\u{2191}\u{2193}", Style::default().fg(theme.accent_soft)),
                Span::styled(" scroll", Style::default().fg(theme.text_dim)),
            ]));

            frame.render_widget(Clear, popup_area);
            frame.render_widget(block, popup_area);
            frame.render_widget(
                Paragraph::new(all_lines).style(Style::default().bg(theme.panel)),
                inner,
            );
        }
        Modal::Picker(picker) => render_picker(frame, area, picker, theme),
        Modal::Question(qs) => render_question(frame, area, qs, theme),
    }
}

fn render_picker(frame: &mut Frame, area: Rect, picker: &PickerState, theme: &Theme) {
    let popup_width = area.width.clamp(48, 80);
    let visible = picker.items.len().clamp(3, 12);
    let popup_h = (visible + 8) as u16;
    let popup_h = popup_h.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_h);

    let is_multi = picker.mode == PickerMode::Multi;
    let hints = if is_multi {
        " \u{2191}\u{2193} navigate  Space toggle  Ctrl+A all  Enter confirm  Esc cancel "
    } else {
        " \u{2191}\u{2193} navigate  Enter select  Esc close "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(format!(" {} ", picker.title))
        .title_style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel));

    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Search bar line
    let search_display = if picker.search.is_empty() {
        "type to filter...".to_string()
    } else {
        picker.search.clone()
    };
    lines.push(Line::from(vec![
        Span::styled(" \u{1F50D} ", Style::default().fg(theme.text_dim).bg(theme.panel)),
        Span::styled(search_display, Style::default().fg(if picker.search.is_empty() { theme.text_dim } else { theme.fg }).bg(theme.panel)),
    ]));

    // Separator after search
    let sep_short = "\u{2500}".repeat(inner_w.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", sep_short), Style::default().fg(theme.border).bg(theme.panel)),
    ]));

    // Item list
    let end = (picker.scroll_offset + visible).min(picker.items.len());

    if picker.scroll_offset > 0 {
        lines.push(Line::from(vec![
            Span::styled("  \u{2191} more ", Style::default().fg(theme.text_dim).bg(theme.panel)),
        ]));
    }

    for i in picker.scroll_offset..end {
        let is_cursor = i == picker.cursor;
        let is_checked = picker.checked.contains(&i);

        let indicator = if is_multi {
            if is_checked { "\u{2611} " } else { "\u{2610} " }
        } else {
            if is_cursor { "\u{25B6} " } else { "  " }
        };

        let bg = if is_cursor { theme.selection } else { theme.panel };
        let fg = if is_cursor { theme.fg } else { theme.text_soft };

        let item = &picker.items[i];
        let text = truncate(&item.text, inner_w.saturating_sub(6));

        let row_style = Style::default().fg(fg).bg(bg);
        let ind_style = Style::default()
            .fg(if is_cursor { theme.accent } else { theme.text_dim })
            .bg(bg);

        lines.push(Line::from(vec![
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(indicator, ind_style),
            Span::styled(text, row_style),
            Span::styled(" ", Style::default().bg(bg)),
        ]));
    }

    if picker.scroll_offset + visible < picker.items.len() {
        lines.push(Line::from(vec![
            Span::styled("  \u{2193} more ", Style::default().fg(theme.text_dim).bg(theme.panel)),
        ]));
    }

    // Fill remaining
    while lines.len() < inner.height.saturating_sub(3) as usize {
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(inner_w.saturating_sub(2)), Style::default().bg(theme.panel)),
        ]));
    }

    // Separator
    let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", sep), Style::default().fg(theme.border)),
    ]));

    // Footer
    let total = picker.all_items.len();
    let shown = picker.items.len();
    let count_str = if is_multi {
        format!("{}/{} sel", picker.checked.len(), total)
    } else if shown < total {
        format!("{}/{} (of {})", picker.cursor + 1, shown, total)
    } else {
        format!("{}/{}", picker.cursor + 1, total)
    };
    let total_w = inner_w.saturating_sub(4);
    let pad = total_w.saturating_sub(count_str.len() + hints.len().saturating_sub(2));
    let pad_str = " ".repeat(pad);

    lines.push(Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.fg)),
        Span::styled(count_str, Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
        Span::styled(pad_str, Style::default().fg(theme.fg)),
        Span::styled(hints, Style::default().fg(theme.text_dim)),
    ]));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}

fn render_question(frame: &mut Frame, area: Rect, qs: &QuestionState, theme: &Theme) {
    let popup_width = area.width.clamp(48, 72);
    let is_custom_mode = qs.is_custom_mode;
    let is_yes_no = qs.options.is_empty();

    let popup_height = if is_custom_mode {
        10u16
    } else if is_yes_no {
        8u16
    } else {
        let visible = qs.options.len().min(10).max(3) + 1;
        (visible + 5) as u16
    };
    let popup_h = popup_height.min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" Question ")
        .title_style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel));

    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Question text
    let wrapped = textwrap::wrap(&qs.question, inner_w.saturating_sub(4));
    for (i, wline) in wrapped.iter().enumerate() {
        let prefix = if i == 0 { "  " } else { "    " };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme.fg)),
            Span::styled(wline.to_string(), Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
        ]));
    }
    lines.push(Line::from(""));

    if is_custom_mode {
        // Custom text input sub-mode
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.text_dim)),
            Span::styled("Enter your answer:", Style::default().fg(theme.text_dim)),
        ]));
        lines.push(Line::from(""));

        let cursor_visible = qs.custom_cursor < qs.custom_input.chars().count();
        let before_cursor: String = qs.custom_input.chars().take(qs.custom_cursor).collect();

        if cursor_visible {
            let after_cursor: String = qs.custom_input.chars().skip(qs.custom_cursor).collect();
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.fg)),
                Span::styled(before_cursor, Style::default().fg(theme.fg).bg(theme.input_bg)),
                Span::styled(
                    qs.custom_input.chars().nth(qs.custom_cursor).map(|c| c.to_string()).unwrap_or_default(),
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.accent)
                        .add_modifier(Modifier::REVERSED),
                ),
                Span::styled(after_cursor, Style::default().fg(theme.fg).bg(theme.input_bg)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.fg)),
                Span::styled(before_cursor, Style::default().fg(theme.fg).bg(theme.input_bg)),
                Span::styled(" ", Style::default().fg(theme.fg).bg(theme.accent).add_modifier(Modifier::REVERSED)),
            ]));
        }

        while lines.len() < inner.height.saturating_sub(3) as usize {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(inner_w.saturating_sub(2)), Style::default().bg(theme.panel)),
            ]));
        }

        let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", sep), Style::default().fg(theme.border)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.fg)),
            Span::styled("Enter", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::styled(" submit  ", Style::default().fg(theme.text_dim)),
            Span::styled("Esc", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::default().fg(theme.text_dim)),
        ]));
    } else if is_yes_no {
        // Yes/No mode
        let pad = inner_w.saturating_sub(14) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(pad), Style::default().fg(theme.fg)),
            Span::styled("Y", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::styled(" Yes  ", Style::default().fg(theme.text_dim)),
            Span::styled("N", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
            Span::styled(" No", Style::default().fg(theme.text_dim)),
        ]));
    } else {
        // Multiple choice mode
        let visible = inner.height.saturating_sub(4) as usize;
        let end = (qs.scroll_offset + visible).min(qs.options.len());

        if qs.scroll_offset > 0 {
            lines.push(Line::from(vec![
                Span::styled("  \u{2191} more ", Style::default().fg(theme.text_dim).bg(theme.panel)),
            ]));
        }

        for i in qs.scroll_offset..end {
            let is_cursor = i == qs.selected;
            let is_custom_entry = qs.allow_custom && i >= qs.options.len().saturating_sub(1);
            let bg = if is_cursor { theme.selection } else { theme.panel };
            let fg = if is_cursor { theme.fg } else { theme.text_soft };

            let indicator = if is_cursor { "\u{25B6} " } else { "  " };
            let label = if is_custom_entry {
                "Custom...".to_string()
            } else {
                qs.options[i].clone()
            };

            lines.push(Line::from(vec![
                Span::styled(" ", Style::default().bg(bg)),
                Span::styled(
                    indicator,
                    Style::default()
                        .fg(if is_cursor { theme.accent } else { theme.text_dim })
                        .bg(bg),
                ),
                Span::styled(label, Style::default().fg(fg).bg(bg)),
                Span::styled(" ", Style::default().bg(bg)),
            ]));
        }

        if qs.scroll_offset + visible < qs.options.len() {
            lines.push(Line::from(vec![
                Span::styled("  \u{2193} more ", Style::default().fg(theme.text_dim).bg(theme.panel)),
            ]));
        }

        while lines.len() < inner.height.saturating_sub(3) as usize {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(inner_w.saturating_sub(2)), Style::default().bg(theme.panel)),
            ]));
        }

        let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", sep), Style::default().fg(theme.border)),
        ]));

        let count = qs.options.len();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}/{}", qs.selected + 1, count),
                Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "    ".to_string(),
                Style::default().fg(theme.fg),
            ),
            Span::styled("\u{2191}\u{2193}", Style::default().fg(theme.accent_soft)),
            Span::styled(" nav  ", Style::default().fg(theme.text_dim)),
            Span::styled("Enter", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::styled(" select  ", Style::default().fg(theme.text_dim)),
            Span::styled("Esc", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::default().fg(theme.text_dim)),
        ]));
    }

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}

fn highlight_json(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    fn value_style(v: &str, theme: &Theme) -> Style {
        let t = v.trim();
        if t.starts_with('"') && t.ends_with('"') {
            Style::default().fg(theme.success)
        } else if t == "true" || t == "false" {
            Style::default().fg(theme.accent_soft)
        } else if t == "null" {
            Style::default().fg(theme.text_dim)
        } else if t.parse::<f64>().is_ok() {
            Style::default().fg(Color::Rgb(255, 203, 107))
        } else {
            Style::default().fg(theme.fg)
        }
    }

    let result: Vec<Line<'static>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim();
            let indent_len = line.len().saturating_sub(trimmed.len());
            let indent = &line[..indent_len];

            if let Some((k, rest)) = trimmed.split_once(':') {
                let key_part = k.trim();
                let after_colon = rest.trim();
                let has_comma = after_colon.ends_with(',');
                let val_str = after_colon.trim_end_matches(',');

                let mut spans = vec![Span::styled(indent.to_string(), Style::default().fg(theme.fg))];
                spans.push(Span::styled(
                    key_part.to_string(),
                    Style::default().fg(theme.accent),
                ));
                spans.push(Span::styled(
                    ": ".to_string(),
                    Style::default().fg(theme.text_dim),
                ));

                if val_str.starts_with('{') || val_str.starts_with('[') {
                    spans.push(Span::styled(
                        val_str.to_string(),
                        Style::default().fg(theme.fg),
                    ));
                } else {
                    spans.push(Span::styled(
                        val_str.to_string(),
                        value_style(val_str, theme),
                    ));
                }
                if has_comma {
                    spans.push(Span::styled(
                        ",".to_string(),
                        Style::default().fg(theme.text_dim),
                    ));
                }
                Line::from(spans)
            } else {
                let t = trimmed;
                let style = if t == "{" || t == "}" || t == "[" || t == "]" {
                    Style::default().fg(theme.text_dim)
                } else if t.starts_with('"') {
                    value_style(t, theme)
                } else {
                    Style::default().fg(theme.fg)
                };
                Line::from(vec![
                    Span::styled(indent.to_string(), Style::default().fg(theme.fg)),
                    Span::styled(t.to_string(), style),
                ])
            }
        })
        .collect();

    result
}

fn provider_label(config: &Config) -> &'static str {
    match config.provider.kind {
        crate::config::ProviderKind::Deepseek => "DeepSeek",
        crate::config::ProviderKind::Openai => "OpenAI",
        crate::config::ProviderKind::Custom => "Custom",
    }
}

const AUTOCOMPLETE_VISIBLE: usize = 8;

fn render_autocomplete(frame: &mut Frame, input_area: Rect, state: &AppState, theme: &Theme) {
    let items = &state.autocomplete.items;
    if items.is_empty() {
        return;
    }
    let total = items.len();

    let visible_count = total.min(AUTOCOMPLETE_VISIBLE);
    let scroll_off = state.autocomplete.scroll_offset.min(total.saturating_sub(1));
    let view_end = (scroll_off + visible_count).min(total);
    let vis_items = &items[scroll_off..view_end];

    let max_name_width = items
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(6) as u16;
    let popup_h = (visible_count + 2) as u16;
    let popup_w = input_area
        .width
        .min(64)
        .max(max_name_width + 28)
        .min(input_area.width);

    let max_y_above = input_area.y.saturating_sub(1);
    let popup_h = popup_h.min(max_y_above.max(2));
    let popup_y = input_area.y.saturating_sub(popup_h);
    let popup_area = Rect::new(input_area.x, popup_y, popup_w, popup_h);

    frame.render_widget(Clear, popup_area);

    let is_file = state.autocomplete.file_mode;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(if is_file { " Files " } else { " Commands " })
        .title_style(
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let sel_style = Style::default()
        .fg(theme.bg)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let name_style = Style::default().fg(theme.accent_soft).bg(theme.panel);
    let desc_style = Style::default().fg(theme.text_soft).bg(theme.panel);
    let usage_style = Style::default().fg(theme.text_dim).bg(theme.panel);
    let muted = Style::default().fg(theme.text_dim).bg(theme.panel);

    let max_name_w = max_name_width as usize;

    let lines: Vec<Line> = vis_items
        .iter()
        .enumerate()
        .map(|(rel, item)| {
            let is_sel = (scroll_off + rel) == state.autocomplete.selected;
            let indicator = if is_sel { "▸ " } else { "  " };
            let prefix = if is_file { "@" } else { "/" };
            let name_full = format!("{prefix}{}", item.name);
            let padding = max_name_w
                .saturating_sub(item.name.chars().count())
                .max(1);
            let pad_str = " ".repeat(padding);

            let mut spans: Vec<Span> = Vec::new();
            if is_sel {
                spans.push(Span::styled(indicator, sel_style));
                spans.push(Span::styled(name_full, sel_style));
                spans.push(Span::styled(pad_str, sel_style));
                let desc_text = if item.description.is_empty() {
                    String::new()
                } else {
                    item.description.clone()
                };
                spans.push(Span::styled(desc_text, sel_style));
                if !item.usage.is_empty() {
                    spans.push(Span::styled("  ", sel_style));
                    spans.push(Span::styled(item.usage.clone(), sel_style));
                }
            } else {
                spans.push(Span::styled(indicator, muted));
                spans.push(Span::styled(name_full, name_style));
                spans.push(Span::styled(pad_str, muted));
                spans.push(Span::styled("  ", muted));
                spans.push(Span::styled(item.description.clone(), desc_style));
                if !item.usage.is_empty() {
                    spans.push(Span::styled("  ", muted));
                    spans.push(Span::styled(item.usage.clone(), usage_style));
                }
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}

fn render_attach_bar(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
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
        let size_str = crate::app::format_size(f.size);
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

fn bigger_title(state: &AppState, theme: &Theme) -> Vec<Span<'static>> {
    let text = "POOPRUSTEEK";
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let pulse = ((state.generation.animation_tick as usize / 5) + index) % 6;
            let color = match pulse {
                0 | 1 => theme.accent_soft,
                2 | 3 => theme.accent,
                _ => theme.success,
            };
            let bright = if index == 0 || index == 5 || index == 9 {
                Modifier::BOLD | Modifier::UNDERLINED
            } else {
                Modifier::BOLD
            };
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(color)
                    .bg(theme.bg)
                    .add_modifier(bright),
            )
        })
        .collect()
}
