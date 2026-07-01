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
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const PANEL_W: usize = 34;

/// How long a cached session tag stays valid before `session::load_local`
/// is asked to re-read the file. The panel repaints every frame while the
/// UI is active, but the on-disk tag only changes on an explicit user
/// action, so a few seconds of staleness is unobservable in practice.
const SESSION_TAG_TTL: Duration = Duration::from_secs(5);

thread_local! {
    /// Session id -> (fetched-at, tag). Avoids re-reading and re-parsing the
    /// whole session JSON file every frame just to show its `tag`.
    static SESSION_TAG_CACHE: RefCell<HashMap<String, (Instant, Option<String>)>> = RefCell::new(HashMap::new());
}

/// The focused session's `tag`, read from disk at most once per
/// [`SESSION_TAG_TTL`] window per session id.
fn cached_session_tag(session_id: &str, config: &Config) -> Option<String> {
    SESSION_TAG_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((fetched_at, tag)) = cache.get(session_id)
            && fetched_at.elapsed() < SESSION_TAG_TTL {
                return tag.clone();
            }
        let tag = session::load_local(session_id, config).ok().and_then(|s| s.tag);
        cache.insert(session_id.to_string(), (Instant::now(), tag.clone()));
        tag
    })
}

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
    let sid = &state.focused().session_id;
    let short_sid = if sid.len() > 17 {
        format!("{}..", crate::util::truncate_at_char_boundary(sid, 17))
    } else {
        sid.clone()
    };
    data_row(&mut lines, "Session", &short_sid, theme);
    blank(&mut lines);

    // ── Session ──
    section_header(&mut lines, "Session", panel_w, theme);
    if let Some(tag) = cached_session_tag(&state.focused().session_id, config) {
        data_row(&mut lines, "Tag", &tag, theme);
    }
    let started = format_time_short(&state.focused().session_started_at);
    data_row(&mut lines, "Started", &started, theme);
    let latest = state.focused().messages.last().map(|m| format_time_short(&m.created_at)).unwrap_or_default();
    data_row(&mut lines, "Latest", &latest, theme);
    data_row(&mut lines, "Messages", &state.focused().messages.len().to_string(), theme);
    blank(&mut lines);

    // ── Tokens ──
    section_header(&mut lines, "Tokens", panel_w, theme);
    let (input_tok, output_tok) = compute_totals(state);
    data_row(&mut lines, "Input", &format_num(input_tok), theme);
    data_row(&mut lines, "Output", &format_num(output_tok), theme);
    let tps = crate::tui::view_model::tokens_per_sec(
        state.focused().generation.last_tokens,
        state.focused().generation.last_duration_secs,
    );
    let speed = if tps > 0.0 {
        format!("{:.1} t/s", tps)
    } else if state.focused().generation.active {
        spinner(state.focused().generation.animation_tick)
    } else {
        "\u{2014}".to_string()
    };
    data_row(&mut lines, "Speed", &speed, theme);
    blank(&mut lines);

    // ── Activity ──
    section_header(&mut lines, "Activity", panel_w, theme);
    let tool_calls = state.focused().messages.iter().filter(|m| m.role == Role::Tool).count();
    data_row(&mut lines, "Tools", &tool_calls.to_string(), theme);
    let file_ops = state.focused().messages.iter()
        .filter(|m| m.role == Role::Tool)
        .filter(|m| m.name.as_deref().is_some_and(|n| matches!(n, "Bash" | "PowerShell" | "ShellInput")))
        .count();
    data_row(&mut lines, "Files", &file_ops.to_string(), theme);
    let think_total: f64 = state.focused().messages.iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.think_elapsed_secs)
        .sum();
    if think_total > 0.0 {
        data_row(&mut lines, "Think", &format!("{:.1}s", think_total), theme);
    }
    let search_count = state.focused().messages.iter().filter(|m| m.search_triggered).count();
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
            let (left, gap, right) = mcp_row_layout(&server.name, server.tool_count, ok, panel_w);
            lines.push(Line::from(vec![
                Span::styled(left, Style::default().fg(theme.fg)),
                Span::styled(" ".repeat(gap), Style::default().fg(theme.fg)),
                Span::styled(right, Style::default().fg(if ok { theme.success } else { theme.warning })),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines).style(Style::default().bg(theme.panel));
    frame.render_widget(paragraph, panel_area);
}

/// Computes `(left, gap, right)` for one MCP server row: `right` is fixed
/// width (tool count + status glyph), `left` is the server name truncated to
/// whatever's left after reserving room for `right` and a minimum 4-column
/// gap, and `gap` is the padding between them. Pulled out of
/// `render_stats_panel` so the width arithmetic — the site of a past
/// underflow panic on long server names — is independently testable.
fn mcp_row_layout(server_name: &str, tool_count: usize, ok: bool, panel_w: usize) -> (String, usize, String) {
    let right = format!(" {} {}", tool_count, if ok { "\u{2713}" } else { "\u{26A0}" });
    let right_len = right.chars().count();
    let left_budget = panel_w.saturating_sub(right_len + 4);
    let left_full = format!(" - {}", server_name);
    let left: String = if left_full.chars().count() > left_budget {
        left_full.chars().take(left_budget).collect()
    } else {
        left_full
    };
    let gap = panel_w
        .saturating_sub(left.chars().count() + right_len)
        .saturating_sub(4);
    (left, gap, right)
}

fn section_header(lines: &mut Vec<Line<'static>>, title: &str, width: usize, theme: &Theme) {
    let prefix = " \u{2501}\u{2501} ".to_string();
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

/// Sums input (user) and output (assistant) token counts across the
/// transcript. Reuses the per-message token cache from `widgets::chat`
/// (shared because a token count doesn't depend on chat viewport width)
/// instead of re-running `estimate_tokens` over every message every frame.
fn compute_totals(state: &AppState) -> (u64, u64) {
    let conversation_id = state.conversations.focused_id().0;
    let mut input = 0u64;
    let mut output = 0u64;
    for (index, msg) in state.focused().messages.iter().enumerate() {
        let tokens = crate::tui::widgets::chat::cached_token_estimate(conversation_id, index, msg) as u64;
        match msg.role {
            Role::User => input += tokens,
            Role::Assistant => output += tokens,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_row_layout_normal_name_matches_prior_visual_layout() {
        // panel_w=34, name="filesystem" (10 chars), tool_count=3, ok=true:
        // left=" - filesystem" (13 chars), right=" 3 \u{2713}" (4 chars),
        // gap = 34 - (13+4) - 4 = 13 — the value the pre-fix formula also
        // produced for this (non-adversarial) input.
        let (left, gap, right) = mcp_row_layout("filesystem", 3, true, 34);
        assert_eq!(left, " - filesystem");
        assert_eq!(right, " 3 \u{2713}");
        assert_eq!(gap, 13);
        assert!(left.chars().count() + gap + right.chars().count() <= 34);
    }

    #[test]
    fn mcp_row_layout_long_name_does_not_underflow_and_fits_panel() {
        // A 40-char server name used to make `gap - 4` underflow (panic in
        // debug, abort on `.repeat()` in release). It must now truncate the
        // left label and keep the row within `panel_w`.
        let long_name = "a".repeat(40);
        let (left, gap, right) = mcp_row_layout(&long_name, 12, false, 34);
        assert!(left.chars().count() + gap + right.chars().count() <= 34);
        assert!(left.chars().count() < long_name.len() + 3, "left label should be truncated");
    }

    #[test]
    fn mcp_row_layout_extremely_long_name_on_narrow_panel_does_not_panic() {
        // Even more adversarial: a very long name on a narrow panel. The
        // point of this test is simply that it returns instead of panicking
        // or aborting (the original bug crashed the whole render thread).
        let long_name = "x".repeat(200);
        let (left, gap, right) = mcp_row_layout(&long_name, 999, true, 20);
        assert!(left.chars().count() + gap + right.chars().count() <= 20);
    }

    #[test]
    fn mcp_row_layout_panel_narrower_than_right_alone_does_not_panic() {
        // `right` itself can exceed `panel_w` (huge tool_count, tiny panel).
        // `left_budget` saturates to 0 and `left` becomes empty rather than
        // underflowing.
        let (left, gap, right) = mcp_row_layout("srv", 999_999, true, 5);
        assert_eq!(left, "");
        assert_eq!(gap, 0);
        assert!(!right.is_empty());
    }

    #[test]
    fn session_tag_cache_misses_gracefully_for_unknown_session() {
        let config = Config::default();
        // A session id that will never exist on disk — must return None,
        // not panic or error out.
        let tag = cached_session_tag("no-such-session-ever-00000000", &config);
        assert_eq!(tag, None);
    }

    #[test]
    fn session_tag_cache_reuses_entry_within_ttl() {
        let config = Config::default();
        let id = "panel-test-ttl-cache-session-id";
        // First call populates the cache (miss on disk -> None, cached).
        let first = cached_session_tag(id, &config);
        // Second call within the TTL window must hit the cache rather than
        // touching disk again; result should be identical.
        let second = cached_session_tag(id, &config);
        assert_eq!(first, second);
    }
}
