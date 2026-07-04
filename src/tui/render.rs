//! Top-level frame assembly: routes to the chat view, the landing screen, or
//! the MCP management view, plus modals and the status bar.
//!
//! The landing screen animates at a fixed low frame rate while idle, which
//! made its `session::list_sessions` call (reads and JSON-parses every
//! session file) a real per-frame cost; see [`landing_sessions_cached`].

use crate::app::AppState;
use crate::app::events::{ConfirmState, Modal, OnboardingState, PickerMode, PickerState, QuestionState, View};
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
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// How long the landing screen's session list stays cached before
/// `session::list_sessions` is asked to re-scan the sessions directory. The
/// landing screen only shows up to 5 sessions and repaints at ~8fps while
/// its logo animates, so a few seconds of staleness (new/renamed sessions
/// appearing slightly late) is not user-visible.
const LANDING_SESSIONS_TTL: Duration = Duration::from_secs(3);

thread_local! {
    static LANDING_SESSIONS_CACHE: RefCell<Option<(Instant, Vec<crate::session::SessionSummary>)>> = const { RefCell::new(None) };
}

/// The landing screen's recent-sessions list, refreshed at most once per
/// [`LANDING_SESSIONS_TTL`] instead of on every animation frame.
fn landing_sessions_cached(config: &Config) -> Vec<crate::session::SessionSummary> {
    LANDING_SESSIONS_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((fetched_at, sessions)) = cache.as_ref()
            && fetched_at.elapsed() < LANDING_SESSIONS_TTL {
                return sessions.clone();
            }
        let sessions = crate::session::list_sessions(config).unwrap_or_default();
        *cache = Some((Instant::now(), sessions.clone()));
        sessions
    })
}

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    let cursor_cell = Cell::new(None);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(String::new()).style(Style::default().bg(theme.bg)),
            area,
        );

        if state.view == View::Onboarding {
            render_onboarding(frame, area, &state.onboarding, state.focused().generation.animation_tick, &theme, &cursor_cell);
            // Bottom status line — /logout and /wipe land here with their result.
            if area.height > 1 {
                let status_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
                render_mini_status(frame, status_area, state, config, &theme);
            }
        } else if state.view == View::Mcp {
            render_mcp_view(frame, area, &state.mcp_status.view, &theme);
        } else if state.focused().messages.is_empty() && !state.focused().generation.active {
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
    if state.modal.is_some() || state.focused().generation.active || state.view == View::Mcp {
        terminal.hide_cursor()?;
    } else {
        // For the onboarding view the cursor is positioned by render_onboarding
        // into cursor_cell — show_cursor makes it visible; hide is handled above.
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

    let sessions = landing_sessions_cached(config);
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
    let title_spans = pulsing_title(state.focused().generation.animation_tick, theme);
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
    render_input_border(frame, border_area, theme);

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

/// Padding between the left/center/right segments of the status bar, sized
/// by display width (not byte length) so multibyte content — Cyrillic
/// status text, emoji in the goal/MCP tags — doesn't overstate how much
/// horizontal space a segment occupies and push `right` off-screen.
fn status_bar_gap(left: &str, center: &str, right: &str, width: u16) -> usize {
    let segments_width = UnicodeWidthStr::width(left) + UnicodeWidthStr::width(center) + UnicodeWidthStr::width(right);
    width.saturating_sub(segments_width as u16).max(1) as usize
}

fn render_mini_status(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme) {
    let provider_name = provider_label(config);
    let model = &config.provider.model;
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

    let status_tag = state.focused().generation.last_status.as_deref().unwrap_or("");

    let center = if state.focused().generation.active {
        
        if let Some(start) = state.focused().generation.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            let tps = view_model::tokens_per_sec(state.focused().generation.last_tokens, elapsed);
            format!(" {} {} ", view_model::spinner_char(state.focused().generation.animation_tick), state.status_message)
                + &format!("({} tok, {:.1}s, {:.0} t/s)", state.focused().generation.last_tokens, elapsed, tps)
        } else {
            format!(" {} {} ", view_model::spinner_char(state.focused().generation.animation_tick), state.status_message)
        }
    } else {
        let mut parts = vec![state.status_message.clone()];
        if !status_tag.is_empty() {
            parts.push(status_tag.to_string());
        }
        let tps = view_model::tokens_per_sec(state.focused().generation.last_tokens, state.focused().generation.last_duration_secs);
        if tps > 0.0 {
            parts.push(format!("{} tok in {:.1}s ({:.0} t/s)", state.focused().generation.last_tokens, state.focused().generation.last_duration_secs, tps));
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
    let session_prefix = format!(" {}", view_model::session_prefix(&state.focused().session_id));
    let right = format!("{} msgs:{} tot:{} | {} ", goal_tag, msg_count, total_tokens, session_prefix);

    let gap = status_bar_gap(&left, &center, &right, area.width);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(theme.text_dim).bg(theme.bg)),
            Span::styled(" ".repeat(gap), Style::default().bg(theme.bg)),
            Span::styled(
                center,
                if state.focused().generation.active {
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
        .title(if mcp.auth_mode { " MCP Authorization " } else { " MCP Server Management " })
        .border_style(Style::default().fg(theme.accent));
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
    let hints = if mcp.auth_mode {
        "  j/k ↑↓ navigate  Enter authorize  Esc/q back  "
    } else if mcp.details_server.is_some() {
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
        Modal::DeleteSessions(st) => render_delete_sessions(frame, area, st, theme),
        Modal::Confirm(cs) => render_confirm(frame, area, cs, theme),
        Modal::McpAdd(state) => render_mcp_add(frame, area, state, theme),
    }
}

/// Render one row of a text box, splitting `buffer` on raw `'\n'` (not
/// `.lines()` — that drops a trailing empty line, which matters when the
/// cursor sits on one) and highlighting the cursor's character in reverse
/// video, same visual treatment as `render_question`'s custom-input mode.
fn push_text_box_lines(lines: &mut Vec<Line<'static>>, buffer: &str, cursor_chars: usize, theme: &Theme, max_rows: usize) {
    let rows: Vec<&str> = buffer.split('\n').collect();

    // Walk the whole buffer once, tracking (row, col) as a char-index
    // counter reaches `cursor_chars` — `\n` both advances the row and
    // resets the column, everything else advances the column.
    let (mut cursor_row, mut cursor_col) = (0usize, 0usize);
    for (i, ch) in buffer.chars().enumerate() {
        if i == cursor_chars {
            break;
        }
        if ch == '\n' {
            cursor_row += 1;
            cursor_col = 0;
        } else {
            cursor_col += 1;
        }
    }

    let max_rows = max_rows.max(1);
    let start = cursor_row.saturating_sub(max_rows.saturating_sub(1));
    let end = (start + max_rows).min(rows.len());

    for (i, row_text) in rows.iter().enumerate() {
        if i < start || i >= end {
            continue;
        }
        if i == cursor_row {
            let chars: Vec<char> = row_text.chars().collect();
            let before: String = chars.iter().take(cursor_col).collect();
            if cursor_col < chars.len() {
                let cursor_ch = chars[cursor_col].to_string();
                let after: String = chars.iter().skip(cursor_col + 1).collect();
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.fg)),
                    Span::styled(before, Style::default().fg(theme.fg).bg(theme.input_bg)),
                    Span::styled(cursor_ch, Style::default().fg(theme.bg).bg(theme.accent).add_modifier(Modifier::REVERSED)),
                    Span::styled(after, Style::default().fg(theme.fg).bg(theme.input_bg)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.fg)),
                    Span::styled(before, Style::default().fg(theme.fg).bg(theme.input_bg)),
                    Span::styled(" ", Style::default().fg(theme.fg).bg(theme.accent).add_modifier(Modifier::REVERSED)),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.fg)),
                Span::styled(row_text.to_string(), Style::default().fg(theme.fg).bg(theme.input_bg)),
            ]));
        }
    }
}

fn render_mcp_add(frame: &mut Frame, area: Rect, state: &crate::app::mcp_add::McpAddState, theme: &Theme) {
    use crate::app::mcp_add::{McpAddState, TransportChoice, WizardStep};

    let popup_width = area.width.clamp(52, 76);
    let popup_h = 16u16.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" Add MCP Server ")
        .title_style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;
    let max_rows = inner.height.saturating_sub(6) as usize;

    let mut lines: Vec<Line> = Vec::new();

    let hint: &str = match state {
        McpAddState::ChooseMethod { selected } => {
            lines.push(Line::from(Span::styled("  Pick a method:", Style::default().fg(theme.text_dim))));
            lines.push(Line::from(""));
            for (i, opt) in ["Paste a JSON config", "Step-by-step wizard"].iter().enumerate() {
                let is_cursor = i == *selected;
                let indicator = if is_cursor { "\u{25B6} " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(theme.fg)),
                    Span::styled(indicator, Style::default().fg(if is_cursor { theme.accent } else { theme.text_dim })),
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
                lines.push(Line::from(Span::styled(format!("  \u{26A0} {err}"), Style::default().fg(theme.error))));
            }
            " Enter add    Shift+Enter newline    Esc back "
        }
        McpAddState::Wizard(wiz) => {
            let (step_num, step_label) = match wiz.step {
                WizardStep::Name => (1, "Server name".to_string()),
                WizardStep::Transport => (2, "Transport".to_string()),
                WizardStep::Primary => (3, wiz.transport.map(TransportChoice::primary_label).unwrap_or("Value").to_string()),
                WizardStep::Extra => (4, wiz.transport.map(TransportChoice::extra_label).unwrap_or("Extra").to_string()),
                WizardStep::Confirm => (5, "Confirm".to_string()),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  Step {step_num}/5 \u{2014} "), Style::default().fg(theme.text_dim)),
                Span::styled(step_label, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(""));

            match wiz.step {
                WizardStep::Name => push_text_box_lines(&mut lines, &wiz.name.buffer, wiz.name.cursor, theme, max_rows),
                WizardStep::Transport => {
                    for (i, choice) in TransportChoice::ALL.iter().enumerate() {
                        let is_cursor = i == wiz.transport_selected;
                        let indicator = if is_cursor { "\u{25B6} " } else { "  " };
                        lines.push(Line::from(vec![
                            Span::styled("  ", Style::default().fg(theme.fg)),
                            Span::styled(indicator, Style::default().fg(if is_cursor { theme.accent } else { theme.text_dim })),
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
                WizardStep::Primary => push_text_box_lines(&mut lines, &wiz.primary.buffer, wiz.primary.cursor, theme, max_rows),
                WizardStep::Extra => push_text_box_lines(&mut lines, &wiz.extra.buffer, wiz.extra.cursor, theme, max_rows),
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
                        lines.push(Line::from(Span::styled("  (+ extra env/headers)", Style::default().fg(theme.text_dim))));
                    }
                }
            }

            if let Some(err) = &wiz.error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(format!("  \u{26A0} {err}"), Style::default().fg(theme.error))));
            }

            match wiz.step {
                WizardStep::Name | WizardStep::Primary => " Enter next    Esc back ",
                WizardStep::Extra => " Enter next (blank = skip)    Shift+Enter newline    Esc back ",
                WizardStep::Transport => " \u{2191}\u{2193} navigate    Enter select    Esc back ",
                WizardStep::Confirm => " Enter add server    Esc back ",
            }
        }
    };

    while lines.len() < inner.height.saturating_sub(2) as usize {
        lines.push(Line::from(""));
    }
    let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
    lines.push(Line::from(Span::styled(format!("  {sep}"), Style::default().fg(theme.border))));
    lines.push(Line::from(Span::styled(hint, Style::default().fg(theme.text_dim))));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(theme.panel)), inner);
}

fn render_delete_sessions(
    frame: &mut Frame,
    area: Rect,
    st: &crate::app::events::DeleteSessionsState,
    theme: &Theme,
) {
    use crate::app::events::{DeleteStage, RemoteListStatus, SessionScope};

    // ── Confirmation stage: compact warning box ──
    if st.stage == DeleteStage::Confirming {
        let popup_width = area.width.clamp(44, 66);
        let popup_h = 8u16.min(area.height.saturating_sub(2));
        let x = (area.width.saturating_sub(popup_width)) / 2;
        let y = (area.height.saturating_sub(popup_h)) / 2;
        let popup_area = Rect::new(x, y, popup_width, popup_h);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.error))
            .title(" \u{1F5D1} Confirm deletion ")
            .title_style(Style::default().fg(theme.error).add_modifier(Modifier::BOLD))
            .style(Style::default().bg(theme.panel));
        let inner = block.inner(popup_area);

        let n = st.confirm_ids.len();
        let scope_text = match st.filter {
            SessionScope::All => "everywhere: DeepSeek account + local files",
            SessionScope::Local => "local files only (account copies stay)",
            SessionScope::Remote => "DeepSeek account only (local files stay)",
        };
        let lines = vec![
            Line::from(Span::styled(
                format!(
                    " Are you sure you want to delete {n} session{}?",
                    if n == 1 { "" } else { "s" }
                ),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(" Scope: {scope_text}"),
                Style::default().fg(theme.text_soft),
            )),
            Line::from(Span::styled(
                " This action is irreversible.",
                Style::default().fg(theme.error),
            )),
            Line::from(""),
            Line::from(Span::styled(
                " Enter/y — delete    n/Esc — back",
                Style::default().fg(theme.text_dim),
            )),
        ];

        frame.render_widget(Clear, popup_area);
        frame.render_widget(block, popup_area);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme.panel)),
            inner,
        );
        return;
    }

    // ── Selection stage: filter tabs + checkbox list ──
    const VISIBLE: usize = 12;
    let visible_entries = st.visible();
    let rows = visible_entries.len().clamp(3, VISIBLE);
    let popup_width = area.width.clamp(52, 84);
    let popup_h = ((rows + 7) as u16).min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning))
        .title(" \u{1F5D1} Delete sessions ")
        .title_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Filter tabs + remote-list status.
    let mut tab_spans: Vec<Span> = vec![Span::styled(" ", Style::default().bg(theme.panel))];
    for scope in [SessionScope::All, SessionScope::Local, SessionScope::Remote] {
        let active = scope == st.filter;
        let style = if active {
            Style::default()
                .fg(theme.accent)
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim).bg(theme.panel)
        };
        tab_spans.push(Span::styled(format!(" {} ", scope.label()), style));
        tab_spans.push(Span::styled(" ", Style::default().bg(theme.panel)));
    }
    let remote_note = match &st.remote_status {
        RemoteListStatus::Loading => " remote: loading\u{2026}".to_string(),
        RemoteListStatus::Failed(e) => format!(" remote: failed ({})", truncate(e, 24)),
        RemoteListStatus::NoProvider => " remote: no provider".to_string(),
        RemoteListStatus::Ready => String::new(),
    };
    if !remote_note.is_empty() {
        tab_spans.push(Span::styled(
            remote_note,
            Style::default().fg(theme.text_dim).bg(theme.panel),
        ));
    }
    lines.push(Line::from(tab_spans));

    let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
    lines.push(Line::from(Span::styled(
        format!("  {sep}"),
        Style::default().fg(theme.border).bg(theme.panel),
    )));

    if visible_entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no sessions under this filter)",
            Style::default().fg(theme.text_dim).bg(theme.panel),
        )));
    }

    let end = (st.scroll_offset + VISIBLE).min(visible_entries.len());
    if st.scroll_offset > 0 {
        lines.push(Line::from(Span::styled(
            "  \u{2191} more ",
            Style::default().fg(theme.text_dim).bg(theme.panel),
        )));
    }
    for (i, entry) in visible_entries
        .iter()
        .enumerate()
        .take(end)
        .skip(st.scroll_offset)
    {
        let is_cursor = i == st.cursor;
        let is_checked = st.checked.contains(&entry.id);
        let bg = if is_cursor { theme.selection } else { theme.panel };

        let checkbox = if is_checked { "\u{2611} " } else { "\u{2610} " };
        let badge = match (entry.local, entry.remote) {
            (true, true) => "[L+G]",
            (true, false) => "[L]  ",
            (false, true) => "[G]  ",
            (false, false) => "[?]  ",
        };
        let date = entry.updated_at.chars().take(10).collect::<String>();
        let title_budget = inner_w.saturating_sub(14 + date.len());
        let title = truncate(&entry.title, title_budget);

        lines.push(Line::from(vec![
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                checkbox,
                Style::default()
                    .fg(if is_checked { theme.warning } else { theme.text_dim })
                    .bg(bg),
            ),
            Span::styled(
                format!("{badge} "),
                Style::default().fg(theme.accent_dim).bg(bg),
            ),
            Span::styled(
                title,
                Style::default()
                    .fg(if is_cursor { theme.fg } else { theme.text_soft })
                    .bg(bg),
            ),
            Span::styled(
                format!("  {date}"),
                Style::default().fg(theme.text_dim).bg(bg),
            ),
        ]));
    }
    if end < visible_entries.len() {
        lines.push(Line::from(Span::styled(
            "  \u{2193} more ",
            Style::default().fg(theme.text_dim).bg(theme.panel),
        )));
    }

    while lines.len() < inner.height.saturating_sub(2) as usize {
        lines.push(Line::from(Span::styled(
            " ".repeat(inner_w),
            Style::default().bg(theme.panel),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!("  {sep}"),
        Style::default().fg(theme.border).bg(theme.panel),
    )));
    let checked_visible = visible_entries
        .iter()
        .filter(|e| st.checked.contains(&e.id))
        .count();
    lines.push(Line::from(Span::styled(
        format!(
            " {checked_visible} selected \u{00B7} \u{2191}\u{2193} move  Space select  A all  Tab filter  Enter delete  Esc close"
        ),
        Style::default().fg(theme.text_dim).bg(theme.panel),
    )));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}

fn render_confirm(frame: &mut Frame, area: Rect, cs: &ConfirmState, theme: &Theme) {
    use crate::app::events::ConfirmLineKind;

    let hint = " Enter/y \u{2014} confirm    n/Esc \u{2014} cancel";
    let content_lines = cs.lines.len() + 2; // lines + blank + hint
    let popup_h = (content_lines as u16 + 4).clamp(6, area.height.saturating_sub(2));
    let max_line_w = cs.lines.iter()
        .map(|l| l.text.len())
        .chain(std::iter::once(hint.len()))
        .max()
        .unwrap_or(40) as u16;
    let popup_width = (max_line_w + 4).clamp(44, area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_h);

    let title = format!(" {} ", cs.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.error))
        .title(title.as_str())
        .title_style(Style::default().fg(theme.error).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup_area);

    let mut lines: Vec<Line> = cs.lines.iter().map(|cl| {
        let style = match cl.kind {
            ConfirmLineKind::Normal => Style::default().fg(theme.fg),
            ConfirmLineKind::Soft => Style::default().fg(theme.text_soft),
            ConfirmLineKind::Dim => Style::default().fg(theme.text_dim),
            ConfirmLineKind::Danger => Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
        };
        Line::from(Span::styled(format!(" {}", cl.text), style))
    }).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {hint}"),
        Style::default().fg(theme.text_dim),
    )));

    frame.render_widget(Clear, popup_area);
    frame.render_widget(block, popup_area);
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
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

        let item = &picker.items[i];
        let bg = if is_cursor { theme.selection } else { theme.panel };
        let fg = if is_cursor {
            theme.fg
        } else if item.warn {
            theme.warning
        } else {
            theme.text_soft
        };

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
        let visible = qs.options.len().clamp(3, 10) + 1;
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
            // `trim_start()`, not `trim()`: `trim()` also strips trailing
            // whitespace, which understates `trimmed.len()` and makes
            // `indent_len` overshoot into the line's actual content
            // (and, since that overshoot byte offset isn't derived from a
            // whitespace prefix, it's no longer guaranteed to land on a
            // char boundary for multibyte content).
            let indent_len = line.len() - line.trim_start().len();
            let indent = crate::util::truncate_at_char_boundary(line, indent_len);

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

fn render_onboarding(
    frame: &mut Frame,
    area: Rect,
    ob: &OnboardingState,
    animation_tick: u64,
    theme: &Theme,
    cursor_cell: &Cell<Option<(u16, u16)>>,
) {
    // ── Layout constants ───────────────────────────────────────────
    const MAX_W: u16 = 72;
    // logo(1) + gap(1) + tagline(1) + gap(1) + steps_panel(7) + gap(1)
    // + input_box(3) + model_line(1) + gap(1) + footer(1) + error_reserve(1)
    const CONTENT_H: u16 = 19;

    let col_w = area.width.min(MAX_W);
    let col_x = area.x + area.width.saturating_sub(col_w) / 2;
    let top_pad = area.height.saturating_sub(CONTENT_H) / 2;

    // Build a vertical layout within the centered column.
    let col_area = Rect::new(col_x, area.y, col_w, area.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),   // 0 — top padding
            Constraint::Length(1),          // 1 — logo
            Constraint::Length(1),          // 2 — gap
            Constraint::Length(1),          // 3 — tagline
            Constraint::Length(1),          // 4 — gap
            Constraint::Length(7),          // 5 — steps panel
            Constraint::Length(1),          // 6 — gap
            Constraint::Length(3),          // 7 — token input box
            Constraint::Length(1),          // 8 — model selector
            Constraint::Length(1),          // 9 — gap
            Constraint::Length(1),          // 10 — footer
            Constraint::Length(1),          // 11 — error line
            Constraint::Min(0),             // 12 — remaining
        ])
        .split(col_area);

    // ── 1. Logo ────────────────────────────────────────────────────
    let title_spans = pulsing_title(animation_tick, theme);
    frame.render_widget(
        Paragraph::new(Line::from(title_spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(theme.bg)),
        chunks[1],
    );

    // ── 3. Tagline ─────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Terminal coding agent · powered by DeepSeek web",
            Style::default().fg(theme.text_dim).bg(theme.bg),
        )))
        .alignment(Alignment::Center),
        chunks[3],
    );

    // ── 5. Steps panel ────────────────────────────────────────────
    // 7 rows total: border-top(1) + 3 step lines(3) + 2 blank rows(2) + border-bottom(1).
    // We use a Block with a title and render paragraphs inside.
    let steps_block = Block::default()
        .title(" Getting started ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.bg))
        .style(Style::default().bg(theme.bg));
    let steps_inner = steps_block.inner(chunks[5]);
    frame.render_widget(steps_block, chunks[5]);

    let steps = [
        ("1.", "Log in at chat.deepseek.com in your browser"),
        ("2.", "DevTools (F12) → Application → Local Storage → copy userToken"),
        ("3.", "Paste it below and press Enter"),
    ];
    let step_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(steps_inner);
    for (i, (num, text)) in steps.iter().enumerate() {
        let line = Line::from(vec![
            Span::styled(
                format!("{num} "),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
            ),
            Span::styled(*text, Style::default().fg(theme.text_soft).bg(theme.bg)),
        ]);
        frame.render_widget(Paragraph::new(line), step_chunks[i]);
    }

    // ── 7. Token input box ────────────────────────────────────────
    let input_block = Block::default()
        .title(" DeepSeek token ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus).bg(theme.input_bg))
        .style(Style::default().bg(theme.input_bg));
    let input_inner = input_block.inner(chunks[7]);
    frame.render_widget(input_block, chunks[7]);

    // Clip the visible portion so the cursor stays in view: show the tail end.
    let box_w = input_inner.width as usize;
    let input_chars: Vec<char> = ob.input.chars().collect();
    let total_chars = input_chars.len();
    // cursor is clamped to [0, total_chars]
    let cursor = ob.cursor.min(total_chars);
    // How many chars fit before the cursor (clipped from the left).
    let visible_start = if cursor >= box_w { cursor - (box_w - 1) } else { 0 };
    let visible_chars: String = input_chars[visible_start..total_chars].iter().collect();
    // Truncate to box width using char-aware slicing (never byte-slice — invariant).
    let display: String = visible_chars.chars().take(box_w).collect();
    let cursor_col = (cursor - visible_start) as u16;

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            display,
            Style::default().fg(theme.fg).bg(theme.input_bg),
        )))
        .style(Style::default().bg(theme.input_bg)),
        input_inner,
    );
    // Place block cursor at the right terminal cell.
    cursor_cell.set(Some((input_inner.x + cursor_col, input_inner.y)));

    // ── 8. Model selector ────────────────────────────────────────
    let (chat_style, reasoner_style, chat_dot, reasoner_dot) = if ob.model_reasoner {
        (
            Style::default().fg(theme.text_dim).bg(theme.bg),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
            "○",
            "●",
        )
    } else {
        (
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
            Style::default().fg(theme.text_dim).bg(theme.bg),
            "●",
            "○",
        )
    };
    let model_line = Line::from(vec![
        Span::styled("Model:  ", Style::default().fg(theme.text_dim).bg(theme.bg)),
        Span::styled(format!("{chat_dot} deepseek-chat"), chat_style),
        Span::styled("   ", Style::default().bg(theme.bg)),
        Span::styled(format!("{reasoner_dot} deepseek-reasoner"), reasoner_style),
        Span::styled("   (Tab to switch)", Style::default().fg(theme.text_dim).bg(theme.bg)),
    ]);
    frame.render_widget(
        Paragraph::new(model_line).alignment(Alignment::Center),
        chunks[8],
    );

    // ── 10. Footer hints ──────────────────────────────────────────
    let footer = Line::from(vec![
        Span::styled(
            "Enter",
            Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD).bg(theme.bg),
        ),
        Span::styled(" — save & start    ", Style::default().fg(theme.text_dim).bg(theme.bg)),
        Span::styled(
            "Ctrl+C",
            Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD).bg(theme.bg),
        ),
        Span::styled(" — quit", Style::default().fg(theme.text_dim).bg(theme.bg)),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        chunks[10],
    );

    // ── 11. Error line ────────────────────────────────────────────
    if let Some(msg) = ob.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.error).bg(theme.bg),
            )))
            .alignment(Alignment::Center),
            chunks[11],
        );
    }
}

/// Animated pulsing logo shared by the landing and onboarding screens.
fn pulsing_title(animation_tick: u64, theme: &Theme) -> Vec<Span<'static>> {
    let text = "POOPRUSTEEK";
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let pulse = ((animation_tick as usize / 5) + index) % 6;
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
                Style::default().fg(color).bg(theme.bg).add_modifier(bright),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bar_gap_ascii_matches_byte_length_math() {
        // Pure-ASCII case: display width equals byte length, so this should
        // match what the old `.len()`-based formula produced.
        let gap = status_bar_gap(" left ", " center ", " right ", 40);
        assert_eq!(gap, 40usize.saturating_sub(6 + 8 + 7));
    }

    #[test]
    fn status_bar_gap_multibyte_uses_display_width_not_byte_length() {
        // Cyrillic text: each character is 2 bytes in UTF-8 but 1 display
        // cell. A byte-length-based gap would undercount available space
        // and push `right` further than it should.
        let left = " статус "; // 8 chars, display width 8 cells, byte len 14 (6 Cyrillic letters x 2 bytes + 2 ASCII spaces)
        let center = " ";
        let right = " right ";
        let gap_by_width = status_bar_gap(left, center, right, 40);

        let byte_len_gap = 40usize.saturating_sub(
            (left.len() + center.len() + right.len()).min(40),
        );
        assert_ne!(
            gap_by_width, byte_len_gap,
            "multibyte content should make the width- and byte-length-based gaps diverge"
        );

        let expected = 40usize.saturating_sub(
            UnicodeWidthStr::width(left) + UnicodeWidthStr::width(center) + UnicodeWidthStr::width(right),
        );
        assert_eq!(gap_by_width, expected);
    }

    #[test]
    fn status_bar_gap_never_zero() {
        // `.max(1)` guards against a fully-collapsed gap even when segments
        // overflow the available width.
        let gap = status_bar_gap(&"x".repeat(100), "y", "z", 10);
        assert_eq!(gap, 1);
    }

    fn theme() -> Theme {
        Theme::default_dark()
    }

    #[test]
    fn highlight_json_indent_trailing_whitespace_does_not_overshoot() {
        // Old bug: `line.trim()` (strips both ends) made `indent_len`
        // overshoot into the actual content when the line had trailing
        // whitespace, since `trimmed.len()` was too small — e.g. for this
        // input (2 leading spaces, 3 trailing) the old formula computed
        // indent_len=5 instead of 2, so the indent span became `"  \"ke"`
        // (bleeding into the key) instead of `"  "`. Assert the exact first
        // span rather than just checking the rendered line contains "key",
        // since the corrupted variant still contains that substring.
        let text = "  \"key\": \"value\",   \n";
        let lines = highlight_json(text, &theme());
        assert_eq!(lines.len(), 1);
        let first_span = &lines[0].spans[0];
        assert_eq!(first_span.content.as_ref(), "  ", "indent span must be exactly the 2 leading spaces");

        let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "  \"key\": \"value\",");
    }

    #[test]
    fn highlight_json_indent_handles_multibyte_before_trailing_space() {
        // A line with multibyte content followed by trailing whitespace
        // must not panic when computing/slicing the indent.
        let text = "  \"名前\": \"値\"   \n";
        let lines = highlight_json(text, &theme());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn highlight_json_plain_lines_still_render() {
        let text = "{\n  \"a\": 1,\n  \"b\": true\n}\n";
        let lines = highlight_json(text, &theme());
        assert_eq!(lines.len(), 4);
    }
}
