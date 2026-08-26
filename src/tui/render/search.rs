//! The `/search` screen (`View::Search`): query line, filter/sort bar,
//! scrollable result list, footer hints. Layout follows the other
//! full-screen views (providers/mcp); the scroll window is derived from
//! the selection so the state stays render-agnostic.

use crate::app::list::{ListWindow, NAV_HINT};
use crate::app::search::{SearchFocus, SearchViewState};
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Rows per result: title line + snippet line + spacer.
const ROWS_PER_ITEM: u16 = 3;

pub(super) fn render_search_view(
    frame: &mut Frame,
    area: Rect,
    view: &SearchViewState,
    theme: &Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // query box
            Constraint::Length(1), // filter/sort bar
            Constraint::Min(1),    // results
            Constraint::Length(3), // footer
        ])
        .split(area);

    render_query_box(frame, chunks[0], view, theme);
    render_filter_bar(frame, chunks[1], view, theme);
    render_results(frame, chunks[2], view, theme);
    render_footer(frame, chunks[3], view, theme);
}

fn render_query_box(frame: &mut Frame, area: Rect, view: &SearchViewState, theme: &Theme) {
    let focused = view.focus == SearchFocus::Query;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Search History ")
        .border_style(Style::default().fg(if focused { theme.accent } else { theme.border }));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut spans = vec![Span::styled(" 🔎 ", Style::default().fg(theme.accent))];
    if view.query.buffer.is_empty() && !focused {
        spans.push(Span::styled(
            "type to search…",
            Style::default().fg(theme.text_dim),
        ));
    } else {
        let chars: Vec<char> = view.query.buffer.chars().collect();
        let cursor = view.query.cursor.min(chars.len());
        let before: String = chars[..cursor].iter().collect();
        spans.push(Span::styled(before, Style::default().fg(theme.fg)));
        if focused {
            // Block cursor: highlight the char under it, or a bar at the end.
            if cursor < chars.len() {
                spans.push(Span::styled(
                    chars[cursor].to_string(),
                    Style::default().fg(theme.bg).bg(theme.accent),
                ));
                let after: String = chars[cursor + 1..].iter().collect();
                spans.push(Span::styled(after, Style::default().fg(theme.fg)));
            } else {
                spans.push(Span::styled("▏", Style::default().fg(theme.accent)));
            }
        } else if cursor < chars.len() {
            let after: String = chars[cursor..].iter().collect();
            spans.push(Span::styled(after, Style::default().fg(theme.fg)));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn render_filter_bar(frame: &mut Frame, area: Rect, view: &SearchViewState, theme: &Theme) {
    let visible = view.visible();
    let right = if view.searching {
        "searching…".to_string()
    } else if view.matches.is_empty() {
        view.status.clone()
    } else if visible.len() == view.matches.len() {
        format!("{} matches", view.matches.len())
    } else {
        format!("{} of {} shown", visible.len(), view.matches.len())
    };

    let tag = |label: &str, value: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!(" {label} "), Style::default().fg(theme.text_dim)),
            Span::styled(
                format!("[{value}]"),
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    };

    let mut spans = Vec::new();
    spans.extend(tag("sort", view.sort.label()));
    spans.extend(tag("role", view.role_filter.label()));
    spans.extend(tag(
        "unique",
        if view.unique_sessions {
            "per session"
        } else {
            "off"
        },
    ));
    spans.push(Span::styled(
        format!("  ·  {right}"),
        Style::default().fg(theme.text_dim),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_results(frame: &mut Frame, area: Rect, view: &SearchViewState, theme: &Theme) {
    let visible = view.visible();
    if visible.is_empty() {
        let message = if view.searching {
            "Searching…"
        } else if view.matches.is_empty() && view.last_query.is_empty() {
            "Type a query and press Enter — semantic + keyword search over all saved sessions."
        } else {
            "Nothing to show (adjust the query or filters)."
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                message,
                Style::default().fg(theme.text_dim),
            )))
            .alignment(Alignment::Center),
            Rect::new(area.x, area.y + area.height / 3, area.width, 1),
        );
        return;
    }

    let capacity = (area.height / ROWS_PER_ITEM) as usize;
    let selected = view.selected.min(visible.len() - 1);
    let window = ListWindow::anchored(selected, visible.len(), capacity);
    let list_focused = view.focus == SearchFocus::Results;

    for (row, slot) in window.range().enumerate() {
        let m = &view.matches[visible[slot]];
        let y = area.y + (row as u16) * ROWS_PER_ITEM;
        let is_selected = slot == selected && list_focused;

        let date = m.timestamp.split('T').next().unwrap_or("—");
        let date = if date.is_empty() { "—" } else { date };
        let marker = if is_selected { "▶ " } else { "  " };
        let role_color = if m.role == "user" {
            theme.accent_soft
        } else {
            theme.success
        };

        let title_style = if is_selected {
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
        };
        let title_line = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.accent)),
            Span::styled(format!("{date}  "), Style::default().fg(theme.text_dim)),
            Span::styled(m.title.clone(), title_style),
            Span::styled(format!("  {}", m.role), Style::default().fg(role_color)),
        ]);
        frame.render_widget(
            Paragraph::new(title_line),
            Rect::new(area.x, y, area.width, 1),
        );

        if y + 1 < area.y + area.height {
            let budget = (area.width as usize).saturating_sub(6).max(16);
            let snippet =
                crate::util::truncate_with_ellipsis(&m.text.replace(['\n', '\r'], " "), budget);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(snippet, Style::default().fg(theme.text_soft)),
                ])),
                Rect::new(area.x, y + 1, area.width, 1),
            );
        }
    }

    // Scroll indicator when the window clips the list.
    if visible.len() > capacity {
        let text = format!(" {}/{} ", selected + 1, visible.len());
        let w = text.chars().count() as u16;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(theme.text_dim),
            ))),
            Rect::new(area.x + area.width.saturating_sub(w), area.y, w, 1),
        );
    }
}

fn render_footer(frame: &mut Frame, area: Rect, view: &SearchViewState, theme: &Theme) {
    let hints = match view.focus {
        SearchFocus::Query => "  Enter search   ↓/Tab results   Esc close  ".to_string(),
        SearchFocus::Results => {
            format!("  {NAV_HINT} move  Enter open  s sort  r role  u unique  Tab query  q close  ")
        }
    };
    let footer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = footer.inner(area);
    frame.render_widget(footer, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints,
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        inner,
    );
}
