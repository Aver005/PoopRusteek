use crate::app::AppState;
use crate::config::Config;
use crate::provider::Role;
use crate::session;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Block},
    Frame,
};

const PANEL_W: usize = 34;

pub fn render_stats_panel(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme) {
    if !state.show_stats_panel {
        return;
    }
    let panel_w = PANEL_W.min(area.width as usize);
    if panel_w < 20 {
        return;
    }
    let panel_area = Rect::new(area.x + area.width - panel_w as u16, area.y, panel_w as u16, area.height);

    // Fill background
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.panel)),
        panel_area,
    );

    // Vertical separator
    let sep_x = panel_area.x - 1;
    if sep_x > area.x {
        let sep_style = Style::default().fg(theme.border).bg(theme.bg);
        for row in 0..panel_area.height {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("\u{2502}", sep_style))),
                Rect::new(sep_x, panel_area.y + row, 1, 1),
            );
        }
    }

    let mut lines: Vec<Line> = Vec::new();

    // ── Model ──
    section_header(&mut lines, "Model", panel_w, theme);
    data_row(&mut lines, "Model", &config.provider.model, theme);
    let sid = &state.current_session_id;
    let short_sid = if sid.len() > 17 {
        format!("{}..", &sid[..17])
    } else {
        sid.clone()
    };
    data_row(&mut lines, "Session", &short_sid, theme);
    blank(&mut lines);

    // ── Session ──
    section_header(&mut lines, "Session", panel_w, theme);
    if let Ok(s) = session::load_local(&state.current_session_id, config) {
        if let Some(ref tag) = s.tag {
            data_row(&mut lines, "Tag", tag, theme);
        }
    }
    let started = format_time_short(&state.session_started_at);
    data_row(&mut lines, "Started", &started, theme);
    let latest = state.messages.last().map(|m| format_time_short(&m.created_at)).unwrap_or_default();
    data_row(&mut lines, "Latest", &latest, theme);
    data_row(&mut lines, "Messages", &state.messages.len().to_string(), theme);
    blank(&mut lines);

    // ── Tokens ──
    section_header(&mut lines, "Tokens", panel_w, theme);
    let (input_tok, output_tok) = compute_totals(state);
    data_row(&mut lines, "Input", &format_num(input_tok), theme);
    data_row(&mut lines, "Output", &format_num(output_tok), theme);
    let tps = crate::tui::view_model::tokens_per_sec(
        state.generation.last_tokens,
        state.generation.last_duration_secs,
    );
    let speed = if tps > 0.0 {
        format!("{:.1} t/s", tps)
    } else if state.generation.active {
        spinner(state.generation.animation_tick)
    } else {
        "\u{2014}".to_string()
    };
    data_row(&mut lines, "Speed", &speed, theme);
    blank(&mut lines);

    // ── Activity ──
    section_header(&mut lines, "Activity", panel_w, theme);
    let tool_calls = state.messages.iter().filter(|m| m.role == Role::Tool).count();
    data_row(&mut lines, "Tools", &tool_calls.to_string(), theme);
    let file_ops = state.messages.iter()
        .filter(|m| m.role == Role::Tool)
        .filter(|m| m.name.as_deref().is_some_and(|n| matches!(n, "Bash" | "PowerShell" | "ShellInput")))
        .count();
    data_row(&mut lines, "Files", &file_ops.to_string(), theme);
    let think_total: f64 = state.messages.iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.think_elapsed_secs)
        .sum();
    if think_total > 0.0 {
        data_row(&mut lines, "Think", &format!("{:.1}s", think_total), theme);
    }
    let search_count = state.messages.iter().filter(|m| m.search_triggered).count();
    if search_count > 0 {
        data_row(&mut lines, "Search", &search_count.to_string(), theme);
    }
    blank(&mut lines);

    // ── MCP Servers ──
    section_header(&mut lines, "MCP", panel_w, theme);
    let enabled_servers: Vec<_> = state.mcp_status.view.servers.iter().filter(|s| s.enabled).collect();
    if enabled_servers.is_empty() {
        data_row(&mut lines, "\u{2713}", "none", theme);
    } else {
        for server in &enabled_servers {
            let ok = server.status == "connected";
            let left = format!(" - {}", server.name);
            let right = format!(" {} {}", server.tool_count, if ok { "\u{2713}" } else { "\u{26A0}" });
            let gap = panel_w.saturating_sub(left.chars().count() + right.chars().count());
            lines.push(Line::from(vec![
                Span::styled(left, Style::default().fg(theme.fg)),
                Span::styled(" ".repeat(gap - 4), Style::default().fg(theme.fg)),
                Span::styled(right, Style::default().fg(if ok { theme.success } else { theme.warning })),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme.panel));
    frame.render_widget(paragraph, panel_area);
}

fn section_header(lines: &mut Vec<Line<'static>>, title: &str, width: usize, theme: &Theme) {
    let prefix = format!(" \u{2501}\u{2501} ");
    let suffix_len = width.saturating_sub(prefix.len() + title.len() + 1);
    let suffix = "\u{2501}".repeat(suffix_len);
    lines.push(Line::from(vec![Span::styled(
        format!("{}{} {}", prefix, title, suffix),
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
    )]));
}

fn data_row(lines: &mut Vec<Line<'static>>, label: &str, value: &str, theme: &Theme) {
    let label_w = label.chars().count().min(10);
    let pad = if label_w < 10 { " ".repeat(10 - label_w) } else { String::new() };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}{}", label, pad),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled(
            value.to_string(),
            Style::default().fg(theme.fg),
        ),
    ]));
}

fn blank(lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(""));
}

fn format_time_short(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        dt.format("%H:%M").to_string()
    } else {
        "...".to_string()
    }
}

fn compute_totals(state: &AppState) -> (u64, u64) {
    let mut input = 0u64;
    let mut output = 0u64;
    for msg in &state.messages {
        match msg.role {
            Role::User => {
                input += crate::provider::estimate_tokens(&msg.content) as u64;
            }
            Role::Assistant => {
                let t = msg.total_tokens.unwrap_or_else(|| crate::provider::estimate_tokens(&msg.content));
                output += t as u64;
            }
            _ => {}
        }
    }
    (input, output)
}

fn format_num(n: u64) -> String {
    if n >= 1000 {
        format!("{}.{}k", n / 1000, (n % 1000) / 100)
    } else {
        n.to_string()
    }
}

fn spinner(tick: u64) -> String {
    match tick % 4 {
        0 => "\u{25D0}".to_string(),
        1 => "\u{25D1}".to_string(),
        2 => "\u{25D1}".to_string(),
        _ => "\u{25D0}".to_string(),
    }
}
