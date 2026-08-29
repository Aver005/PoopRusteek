//! Interaction popups drawn over whatever view is active: tool approval,
//! confirm, picker, question, session deletion, and the input
//! autocomplete dropdown. All of them share the popup skeleton from
//! [`super::popup`].

use super::popup::{
    center_popup, draw_popup, fill_panel_space, modal_block, overflow_line, push_text_box_lines,
    separator_line,
};
use super::util::{highlight_json, truncate};
use crate::app::AppState;
use crate::app::events::{ConfirmState, Modal, PickerMode, PickerState, QuestionState};
use crate::app::list::{NAV_HINT, NAV_HINT_ARROWS};
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub(super) fn render_modal(frame: &mut Frame, area: Rect, modal: &Modal, theme: &Theme) {
    match modal {
        Modal::ToolApproval {
            tool_name,
            arguments,
            scroll_offset,
            always_allow,
        } => render_tool_approval(
            frame,
            area,
            tool_name,
            arguments,
            *scroll_offset,
            *always_allow,
            theme,
        ),
        Modal::Picker(picker) => render_picker(frame, area, picker, theme),
        Modal::Question(qs) => render_question(frame, area, qs, theme),
        Modal::DeleteSessions(st) => render_delete_sessions(frame, area, st, theme),
        Modal::Confirm(cs) => render_confirm(frame, area, cs, theme),
        Modal::McpAdd(state) => super::mcp::render_mcp_add(frame, area, state, theme),
        Modal::ProviderAdd(state) => {
            super::providers::render_provider_add(frame, area, state, theme)
        }
    }
}

fn render_tool_approval(
    frame: &mut Frame,
    area: Rect,
    tool_name: &str,
    arguments: &str,
    scroll_offset: usize,
    always_allow: bool,
    theme: &Theme,
) {
    let popup_width = area.width.clamp(50, 72);
    let max_height = area.height.saturating_sub(4);
    let content_height = arguments
        .lines()
        .count()
        .max(4)
        .min(max_height.saturating_sub(6) as usize);
    let popup_height = (content_height + 8).min(max_height as usize) as u16;
    let popup_area = center_popup(area, popup_width, popup_height);

    let block = modal_block(" \u{26A0} Tool Call ", theme.warning, theme);
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
            tool_name.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    all_lines.push(Line::from(""));

    // Arguments header
    all_lines.push(Line::from(vec![Span::styled(
        "  \u{1F4CB} Arguments:",
        Style::default().fg(theme.text_dim),
    )]));

    // Parse and format arguments with JSON highlighting
    let arg_lines = highlight_json(arguments, theme);
    let max_visible = inner_h.saturating_sub(6);
    let total_args = arg_lines.len();
    let scroll = scroll_offset;
    let display_lines: Vec<&Line> = if total_args > max_visible {
        let end = (scroll + max_visible).min(total_args);
        arg_lines[scroll..end].iter().collect()
    } else {
        arg_lines.iter().collect()
    };

    for line in &display_lines {
        let mut line_spans = vec![Span::styled("  ", Style::default().fg(theme.fg))];
        for span in &line.spans {
            line_spans.push(Span::styled(span.content.clone(), span.style));
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
        all_lines.push(Line::from(vec![Span::styled(
            format!("  {}", indicator),
            Style::default().fg(theme.text_dim),
        )]));
    }

    fill_panel_space(&mut all_lines, inner_h.saturating_sub(3));
    all_lines.push(separator_line(inner_w, theme));

    // Always allow checkbox
    let check = if always_allow { "\u{2611}" } else { "\u{2610}" };
    all_lines.push(Line::from(vec![Span::styled(
        format!("  {} Always allow (saved, survives restart)", check),
        if always_allow {
            Style::default().fg(theme.success)
        } else {
            Style::default().fg(theme.text_dim)
        },
    )]));

    // Key bindings
    all_lines.push(Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.fg)),
        Span::styled(
            "Y",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" allow  ", Style::default().fg(theme.text_dim)),
        Span::styled(
            "N",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" deny  ", Style::default().fg(theme.text_dim)),
        Span::styled(
            "A",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" toggle  ", Style::default().fg(theme.text_dim)),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(theme.accent_soft)),
        Span::styled(" scroll", Style::default().fg(theme.text_dim)),
    ]));

    draw_popup(frame, popup_area, inner, block, all_lines, theme);
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
        let popup_area = center_popup(area, popup_width, popup_h);

        let block = modal_block(" \u{1F5D1} Confirm deletion ", theme.error, theme);
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

        draw_popup(frame, popup_area, inner, block, lines, theme);
        return;
    }

    // ── Selection stage: filter tabs + checkbox list ──
    const VISIBLE: usize = 12;
    let visible_entries = st.visible();
    let rows = visible_entries.len().clamp(3, VISIBLE);
    let popup_width = area.width.clamp(52, 84);
    let popup_h = ((rows + 7) as u16).min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_width, popup_h);

    let block = modal_block(" \u{1F5D1} Delete sessions ", theme.warning, theme);
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

    lines.push(separator_line(inner_w, theme));

    if visible_entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no sessions under this filter)",
            Style::default().fg(theme.text_dim).bg(theme.panel),
        )));
    }

    let window = st.cursor.window(visible_entries.len(), VISIBLE);
    if window.more_above {
        lines.push(overflow_line(true, theme));
    }
    for (i, entry) in visible_entries
        .iter()
        .enumerate()
        .take(window.end)
        .skip(window.start)
    {
        let is_cursor = i == st.cursor.selected;
        let is_checked = st.checked.contains(&entry.id);
        let bg = if is_cursor {
            theme.selection
        } else {
            theme.panel
        };

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
                    .fg(if is_checked {
                        theme.warning
                    } else {
                        theme.text_dim
                    })
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
    if window.more_below {
        lines.push(overflow_line(false, theme));
    }

    fill_panel_space(&mut lines, inner.height.saturating_sub(2) as usize);
    lines.push(separator_line(inner_w, theme));
    let checked_visible = visible_entries
        .iter()
        .filter(|e| st.checked.contains(&e.id))
        .count();
    lines.push(Line::from(Span::styled(
        format!(
            " {checked_visible} selected \u{00B7} {NAV_HINT} move  Space select  A all  Tab filter  Enter delete  Esc close"
        ),
        Style::default().fg(theme.text_dim).bg(theme.panel),
    )));

    draw_popup(frame, popup_area, inner, block, lines, theme);
}

fn render_confirm(frame: &mut Frame, area: Rect, cs: &ConfirmState, theme: &Theme) {
    use crate::app::events::ConfirmLineKind;

    let hint = " Enter/y \u{2014} confirm    n/Esc \u{2014} cancel";
    let content_lines = cs.lines.len() + 2; // lines + blank + hint
    let popup_h = (content_lines as u16 + 4).clamp(6, area.height.saturating_sub(2));
    let max_line_w = cs
        .lines
        .iter()
        .map(|l| l.text.len())
        .chain(std::iter::once(hint.len()))
        .max()
        .unwrap_or(40) as u16;
    let popup_width = (max_line_w + 4).clamp(44, area.width.saturating_sub(4));
    let popup_area = center_popup(area, popup_width, popup_h);

    let block = modal_block(format!(" {} ", cs.title), theme.error, theme);
    let inner = block.inner(popup_area);

    let mut lines: Vec<Line> = cs
        .lines
        .iter()
        .map(|cl| {
            let style = match cl.kind {
                ConfirmLineKind::Normal => Style::default().fg(theme.fg),
                ConfirmLineKind::Soft => Style::default().fg(theme.text_soft),
                ConfirmLineKind::Dim => Style::default().fg(theme.text_dim),
                ConfirmLineKind::Danger => Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            };
            Line::from(Span::styled(format!(" {}", cl.text), style))
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {hint}"),
        Style::default().fg(theme.text_dim),
    )));

    draw_popup(frame, popup_area, inner, block, lines, theme);
}

fn render_picker(frame: &mut Frame, area: Rect, picker: &PickerState, theme: &Theme) {
    let popup_width = area.width.clamp(48, 80);
    let visible = picker.items.len().clamp(3, 12);
    let popup_h = (visible + 8) as u16;
    let popup_h = popup_h.min(area.height.saturating_sub(2));
    let popup_area = center_popup(area, popup_width, popup_h);

    let is_multi = picker.mode == PickerMode::Multi;
    // Пикер фильтруется набором текста, поэтому навигация только стрелками.
    let hints = if is_multi {
        format!(" {NAV_HINT_ARROWS} nav  Space toggle  Ctrl+A all  Enter ok  Esc cancel ")
    } else {
        format!(" {NAV_HINT_ARROWS} navigate  Enter select  Esc close ")
    };

    let block = modal_block(format!(" {} ", picker.title), theme.accent, theme);
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
        Span::styled(
            " \u{1F50D} ",
            Style::default().fg(theme.text_dim).bg(theme.panel),
        ),
        Span::styled(
            search_display,
            Style::default()
                .fg(if picker.search.is_empty() {
                    theme.text_dim
                } else {
                    theme.fg
                })
                .bg(theme.panel),
        ),
    ]));

    // Separator after search
    lines.push(separator_line(inner_w, theme));

    // Item list
    let window = picker.cursor.window(picker.items.len(), visible);

    if window.more_above {
        lines.push(overflow_line(true, theme));
    }

    for i in window.range() {
        let is_cursor = i == picker.cursor.selected;
        let is_checked = picker.checked.contains(&i);

        let indicator = if is_multi {
            if is_checked { "\u{2611} " } else { "\u{2610} " }
        } else {
            if is_cursor { "\u{25B6} " } else { "  " }
        };

        let item = &picker.items[i];
        let bg = if is_cursor {
            theme.selection
        } else {
            theme.panel
        };
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
            .fg(if is_cursor {
                theme.accent
            } else {
                theme.text_dim
            })
            .bg(bg);

        lines.push(Line::from(vec![
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(indicator, ind_style),
            Span::styled(text, row_style),
            Span::styled(" ", Style::default().bg(bg)),
        ]));
    }

    if window.more_below {
        lines.push(overflow_line(false, theme));
    }

    fill_panel_space(&mut lines, inner.height.saturating_sub(3) as usize);
    lines.push(separator_line(inner_w, theme));

    // Footer
    let total = picker.all_items.len();
    let shown = picker.items.len();
    let count_str = if is_multi {
        format!("{}/{} sel", picker.checked.len(), total)
    } else if shown < total {
        format!("{}/{} (of {})", picker.cursor.selected + 1, shown, total)
    } else {
        format!("{}/{}", picker.cursor.selected + 1, total)
    };
    let total_w = inner_w.saturating_sub(4);
    let pad = total_w.saturating_sub(count_str.len() + hints.len().saturating_sub(2));
    let pad_str = " ".repeat(pad);

    lines.push(Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.fg)),
        Span::styled(
            count_str,
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(pad_str, Style::default().fg(theme.fg)),
        Span::styled(hints, Style::default().fg(theme.text_dim)),
    ]));

    draw_popup(frame, popup_area, inner, block, lines, theme);
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
    let popup_area = center_popup(area, popup_width, popup_h);

    let block = modal_block(" Question ", theme.accent, theme);
    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Question text
    let wrapped = textwrap::wrap(&qs.question, inner_w.saturating_sub(4));
    for (i, wline) in wrapped.iter().enumerate() {
        let prefix = if i == 0 { "  " } else { "    " };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme.fg)),
            Span::styled(
                wline.to_string(),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
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

        // Shared text-box renderer (cursor highlight, `\n`-aware): the
        // hand-rolled single-line variant this replaces glued embedded
        // newlines (possible via paste) into one line.
        push_text_box_lines(&mut lines, &qs.custom_input, qs.custom_cursor, theme, 1);

        fill_panel_space(&mut lines, inner.height.saturating_sub(3) as usize);
        lines.push(separator_line(inner_w, theme));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.fg)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" submit  ", Style::default().fg(theme.text_dim)),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(theme.text_dim)),
        ]));
    } else if is_yes_no {
        // Yes/No mode
        let pad = inner_w.saturating_sub(14) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(pad), Style::default().fg(theme.fg)),
            Span::styled(
                "Y",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Yes  ", Style::default().fg(theme.text_dim)),
            Span::styled(
                "N",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" No", Style::default().fg(theme.text_dim)),
        ]));
    } else {
        // Multiple choice mode
        let visible = inner.height.saturating_sub(4) as usize;
        let window = qs.cursor.window(qs.options.len(), visible);

        if window.more_above {
            lines.push(overflow_line(true, theme));
        }

        for i in window.range() {
            let is_cursor = i == qs.cursor.selected;
            let is_custom_entry = qs.allow_custom && i >= qs.options.len().saturating_sub(1);
            let bg = if is_cursor {
                theme.selection
            } else {
                theme.panel
            };
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
                        .fg(if is_cursor {
                            theme.accent
                        } else {
                            theme.text_dim
                        })
                        .bg(bg),
                ),
                Span::styled(label, Style::default().fg(fg).bg(bg)),
                Span::styled(" ", Style::default().bg(bg)),
            ]));
        }

        if window.more_below {
            lines.push(overflow_line(false, theme));
        }

        fill_panel_space(&mut lines, inner.height.saturating_sub(3) as usize);
        lines.push(separator_line(inner_w, theme));

        let count = qs.options.len();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}/{}", qs.cursor.selected + 1, count),
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("    ".to_string(), Style::default().fg(theme.fg)),
            Span::styled(NAV_HINT, Style::default().fg(theme.accent_soft)),
            Span::styled(" nav  ", Style::default().fg(theme.text_dim)),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" select  ", Style::default().fg(theme.text_dim)),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::default().fg(theme.text_dim)),
        ]));
    }

    draw_popup(frame, popup_area, inner, block, lines, theme);
}

const AUTOCOMPLETE_VISIBLE: usize = 8;

pub(super) fn render_autocomplete(
    frame: &mut Frame,
    input_area: Rect,
    state: &AppState,
    theme: &Theme,
) {
    let items = &state.autocomplete.items;
    if items.is_empty() {
        return;
    }
    let total = items.len();

    let window = state
        .autocomplete
        .cursor
        .window(total, AUTOCOMPLETE_VISIBLE);
    let scroll_off = window.start;
    let vis_items = &items[window.range()];

    let max_name_width = items
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(6) as u16;
    let popup_h = (vis_items.len() + 2) as u16;
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
            let is_sel = (scroll_off + rel) == state.autocomplete.cursor.selected;
            let indicator = if is_sel { "▸ " } else { "  " };
            let prefix = if is_file { "@" } else { "/" };
            let name_full = format!("{prefix}{}", item.name);
            let padding = max_name_w.saturating_sub(item.name.chars().count()).max(1);
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
