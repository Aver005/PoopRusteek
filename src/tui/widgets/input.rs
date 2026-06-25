use crate::app::AppState;
use crate::app::char_to_byte_pos;
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::cell::Cell;
use unicode_width::UnicodeWidthStr;

pub fn render_input(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    is_landing: bool,
    cursor_out: &Cell<Option<(u16, u16)>>,
) {
    let input = &state.input_buffer;
    let cursor = state.input_cursor.min(input.chars().count());
    let top_pad = 1;

    let sep_style = Style::default().fg(theme.accent).bg(theme.input_bg);
    let empty_style = Style::default().bg(theme.input_bg);

    let sel: Option<(usize, usize)> = state.input_selection_anchor.map(|anchor| {
        let (start, end) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
        (char_to_byte_pos(input, start), char_to_byte_pos(input, end))
    });

    let logical: Vec<&str> = input.split('\n').collect();
    let mut line_starts: Vec<usize> = Vec::with_capacity(logical.len());
    let mut acc = 0usize;
    for (i, seg) in logical.iter().enumerate() {
        line_starts.push(acc);
        acc += seg.chars().count();
        if i < logical.len() - 1 {
            acc += 1;
        }
    }
    let cursor_row = line_starts.iter().rposition(|&s| s <= cursor).unwrap_or(0);

    let visible_rows = area.height as usize;
    let total_rows = logical.len();
    let top_row = if total_rows <= visible_rows.saturating_sub(top_pad) {
        0
    } else {
        cursor_row
            .saturating_sub(visible_rows.saturating_sub(1 + top_pad))
            .min(total_rows.saturating_sub(visible_rows.saturating_sub(top_pad)))
    };

    let mut rendering_lines: Vec<Line> = Vec::with_capacity(visible_rows);
    let indent_width = 5usize;
    let blank = " ".repeat(area.width.saturating_sub(indent_width as u16) as usize);

    if input.is_empty() {
        for i in 0..visible_rows {
            if i == top_pad {
                let text = if is_landing {
                    "Ask anything \u{2014} describe a task, debug an issue, or ask a question..."
                } else {
                    "Type a message or / to see commands..."
                };
                rendering_lines.push(Line::from(vec![
                    Span::styled(" >  ", sep_style),
                    Span::styled(text, Style::default().fg(theme.text_dim).bg(theme.input_bg)),
                ]));
            } else {
                rendering_lines.push(Line::from(vec![
                    Span::styled(&blank, empty_style),
                ]));
            }
        }
    } else {
        for ri in 0..top_pad.min(visible_rows) {
            rendering_lines.push(Line::from(vec![
                Span::styled(&blank, empty_style),
            ]));
        }
        let indices: Vec<usize> = line_starts.iter().enumerate()
            .filter(|&(ri, _)| {
                if top_pad + ri < top_row {
                    return false;
                }
                top_pad + ri - top_row < visible_rows
            })
            .map(|(ri, _)| ri)
            .collect();
        let max_content = visible_rows.saturating_sub(top_pad);
        for &row_idx in indices.iter().take(max_content) {
            let seg = logical[row_idx];
            let seg_byte_start = char_to_byte_pos(input, line_starts[row_idx]);
            let fg = Style::default().fg(theme.fg).bg(theme.input_bg);
            let sel_style = Style::default()
                .fg(theme.fg)
                .bg(theme.selection)
                .add_modifier(Modifier::REVERSED);

            let mut spans: Vec<Span> = Vec::new();
            if row_idx == 0 {
                spans.push(Span::styled(" >  ", Style::default().fg(theme.accent).bg(theme.input_bg)));
            } else {
                spans.push(Span::styled("     ", empty_style));
            }

            let mut chunk_start = 0usize;
            let seg_bytes = seg.as_bytes();
            let mut idx = 0usize;
            while idx < seg_bytes.len() {
                let Some(ch) = seg[idx..].chars().next() else { break };
                let ch_len = ch.len_utf8();
                let abs_byte = seg_byte_start + idx;
                let in_sel = match sel {
                    Some((s, e)) => abs_byte >= s && abs_byte < e,
                    None => false,
                };
                let mut j = idx + ch_len;
                while j < seg_bytes.len() {
                    let next_byte = seg_byte_start + j;
                    let next_in = match sel {
                        Some((s, e)) => next_byte >= s && next_byte < e,
                        None => false,
                    };
                    if next_in != in_sel {
                        break;
                    }
                    let Some(nch) = seg[j..].chars().next() else { break };
                    j += nch.len_utf8();
                }
                let text = &seg[chunk_start..j];
                spans.push(Span::styled(text, if in_sel { sel_style } else { fg }));
                chunk_start = j;
                idx = j;
            }
            if idx == 0 {
                spans.push(Span::styled("", fg));
            }
            rendering_lines.push(Line::from(spans));
        }
        while rendering_lines.len() < visible_rows {
            rendering_lines.push(Line::from(vec![
                Span::styled(&blank, empty_style),
            ]));
        }
    }

    let paragraph = Paragraph::new(rendering_lines)
        .style(Style::default().bg(theme.input_bg));
    frame.render_widget(paragraph, area);

    let pos = cursor_pos_inner(input, cursor, area, state);
    if let Some((x, y)) = pos {
        frame.set_cursor_position((x, y));
        cursor_out.set(Some((x, y)));
    } else {
        cursor_out.set(None);
    }
}

fn cursor_pos_inner(input: &str, cursor: usize, area: Rect, state: &AppState) -> Option<(u16, u16)> {
    if state.is_generating || state.modal.is_some() {
        return None;
    }
    let top_pad = 1;
    let logical: Vec<&str> = input.split('\n').collect();
    let mut line_starts = Vec::with_capacity(logical.len());
    let mut acc = 0usize;
    for (i, seg) in logical.iter().enumerate() {
        line_starts.push(acc);
        acc += seg.chars().count();
        if i < logical.len() - 1 { acc += 1; }
    }
    let cursor_row = line_starts.iter().rposition(|&s| s <= cursor).unwrap_or(0);
    let cursor_col = cursor - line_starts[cursor_row];
    let visible_rows = area.height as usize;
    let total_rows = logical.len();
    let top_row = if total_rows <= visible_rows.saturating_sub(top_pad) {
        0
    } else {
        cursor_row
            .saturating_sub(visible_rows.saturating_sub(1 + top_pad))
            .min(total_rows.saturating_sub(visible_rows.saturating_sub(top_pad)))
    };
    let screen_row = top_pad + cursor_row.saturating_sub(top_row);
    let row_text = logical.get(cursor_row).copied().unwrap_or("");
    let before_cursor_chars: Vec<char> = row_text.chars().take(cursor_col).collect();
    let before_cursor_str: String = before_cursor_chars.iter().collect();
    let indent = if cursor_row == 0 { 4 } else { 5 };
    let h_offset = u16::try_from(UnicodeWidthStr::width(before_cursor_str.as_str()))
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(indent as u16));
    let x = area.x + indent as u16 + h_offset;
    let cursor_y = area.y.saturating_add(screen_row as u16);
    let cursor_y = cursor_y.clamp(area.y, area.bottom().saturating_sub(1));
    Some((x, cursor_y))
}

#[allow(dead_code)]
pub fn cursor_pos(state: &AppState, area: Rect) -> Option<(u16, u16)> {
    let input = &state.input_buffer;
    let cursor = state.input_cursor.min(input.chars().count());
    cursor_pos_inner(input, cursor, area, state)
}
