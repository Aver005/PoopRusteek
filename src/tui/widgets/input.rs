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

struct VisualLine {
    logical_idx: usize,
    char_start: usize,
    char_end: usize,
}

fn build_visual_lines(input: &str, area_width: u16) -> Vec<VisualLine> {
    let logical: Vec<&str> = input.split('\n').collect();
    let mut visual = Vec::new();
    for (li, seg) in logical.iter().enumerate() {
        let chars: Vec<char> = seg.chars().collect();
        if chars.is_empty() {
            visual.push(VisualLine { logical_idx: li, char_start: 0, char_end: 0 });
            continue;
        }
        let mut offset = 0;
        while offset < chars.len() {
            let indent = if li == 0 && offset == 0 { 4 } else { 5 };
            let max_width = (area_width as usize).saturating_sub(indent);
            if max_width == 0 {
                let end = chars.len();
                visual.push(VisualLine { logical_idx: li, char_start: offset, char_end: end });
                break;
            }
            let mut width = 0usize;
            let mut end = offset;
            while end < chars.len() {
                let w = UnicodeWidthStr::width(chars[end].to_string().as_str()).max(1);
                if width + w > max_width && end > offset {
                    break;
                }
                width += w;
                end += 1;
            }
            visual.push(VisualLine { logical_idx: li, char_start: offset, char_end: end });
            offset = end;
        }
    }
    visual
}

fn line_starts_from_logical(logical: &[&str]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(logical.len());
    let mut acc = 0usize;
    for (i, seg) in logical.iter().enumerate() {
        starts.push(acc);
        acc += seg.chars().count();
        if i < logical.len() - 1 {
            acc += 1;
        }
    }
    starts
}

pub fn render_input(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    is_landing: bool,
    cursor_out: &Cell<Option<(u16, u16)>>,
) {
    let input = &state.input.buffer;
    let cursor = state.input.cursor.min(input.chars().count());
    let top_pad = 1;

    let sep_style = Style::default().fg(theme.accent).bg(theme.input_bg);
    let empty_style = Style::default().bg(theme.input_bg);

    let sel: Option<(usize, usize)> = state.input.selection_anchor.map(|anchor| {
        let (start, end) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
        (char_to_byte_pos(input, start), char_to_byte_pos(input, end))
    });

    let logical: Vec<&str> = input.split('\n').collect();
    let line_starts = line_starts_from_logical(&logical);
    let visual = build_visual_lines(input, area.width);

    let cursor_visual = visual.iter().rposition(|v| {
        let log_start = line_starts[v.logical_idx];
        let abs_end = log_start + (v.char_end - v.char_start);
        cursor >= log_start + v.char_start && cursor <= abs_end
    }).unwrap_or(0);

    let visible_rows = area.height as usize;
    let total_rows = visual.len();
    let top_row = if total_rows <= visible_rows.saturating_sub(top_pad) {
        0
    } else {
        cursor_visual
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
        for _ri in 0..top_pad.min(visible_rows) {
            rendering_lines.push(Line::from(vec![
                Span::styled(&blank, empty_style),
            ]));
        }
        let indices: Vec<usize> = (0..visual.len())
            .filter(|&vi| {
                if top_pad + vi < top_row {
                    return false;
                }
                top_pad + vi - top_row < visible_rows
            })
            .collect();
        let max_content = visible_rows.saturating_sub(top_pad);
        for &vi in indices.iter().take(max_content) {
            let vline = &visual[vi];
            let seg = logical[vline.logical_idx];

            let fg = Style::default().fg(theme.fg).bg(theme.input_bg);
            let sel_style = Style::default()
                .fg(theme.fg)
                .bg(theme.selection)
                .add_modifier(Modifier::REVERSED);

            let mut spans: Vec<Span> = Vec::new();
            if vline.logical_idx == 0 && vline.char_start == 0 {
                spans.push(Span::styled(" >  ", Style::default().fg(theme.accent).bg(theme.input_bg)));
            } else {
                spans.push(Span::styled("     ", empty_style));
            }

            let seg_byte_start = char_to_byte_pos(input, line_starts[vline.logical_idx]);
            let byte_chunk_start = char_to_byte_pos(seg, vline.char_start);
            let byte_chunk_end = char_to_byte_pos(seg, vline.char_end);
            let mut idx = byte_chunk_start;
            while idx < byte_chunk_end.min(seg.len()) {
                let Some(ch) = seg[idx..].chars().next() else { break };
                let ch_len = ch.len_utf8();
                let abs_byte = seg_byte_start + idx;
                let in_sel = match sel {
                    Some((s, e)) => abs_byte >= s && abs_byte < e,
                    None => false,
                };
                let mut j = idx + ch_len;
                while j < byte_chunk_end.min(seg.len()) {
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
                let byte_end = j;
                let text = &seg[idx..byte_end];
                spans.push(Span::styled(text, if in_sel { sel_style } else { fg }));
                idx = j;
            }
            if idx == char_to_byte_pos(seg, vline.char_start) {
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
    let line_starts = line_starts_from_logical(&logical);
    let visual = build_visual_lines(input, area.width);

    let cursor_visual = visual.iter().rposition(|v| {
        let log_start = line_starts.get(v.logical_idx).copied().unwrap_or(0);
        let abs_end = log_start + (v.char_end - v.char_start);
        let abs_start = log_start + v.char_start;
        cursor >= abs_start && cursor <= abs_end
    }).unwrap_or(0);
    let cursor_logical = visual[cursor_visual].logical_idx;
    let cursor_visual_in_logical = visual[..=cursor_visual]
        .iter()
        .filter(|v| v.logical_idx == cursor_logical)
        .count()
        - 1;
    let cursor_col_in_logical = cursor - line_starts[cursor_logical];

    let visible_rows = area.height as usize;
    let total_rows = visual.len();
    let top_row = if total_rows <= visible_rows.saturating_sub(top_pad) {
        0
    } else {
        cursor_visual
            .saturating_sub(visible_rows.saturating_sub(1 + top_pad))
            .min(total_rows.saturating_sub(visible_rows.saturating_sub(top_pad)))
    };
    let screen_row = top_pad + cursor_visual.saturating_sub(top_row);
    let indent = if cursor_logical == 0 && cursor_visual_in_logical == 0 { 4 } else { 5 };

    let seg = logical[cursor_logical];
    let vchar_start = visual[cursor_visual].char_start;
    let before_cursor_chars: String = seg.chars().skip(vchar_start).take(cursor_col_in_logical - vchar_start).collect();
    let h_offset = u16::try_from(UnicodeWidthStr::width(before_cursor_chars.as_str()))
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(indent as u16));
    let x = area.x + indent as u16 + h_offset;
    let cursor_y = area.y.saturating_add(screen_row as u16);
    let cursor_y = cursor_y.clamp(area.y, area.bottom().saturating_sub(1));
    Some((x, cursor_y))
}

#[allow(dead_code)]
pub fn cursor_pos(state: &AppState, area: Rect) -> Option<(u16, u16)> {
    let input = &state.input.buffer;
    let cursor = state.input.cursor.min(input.chars().count());
    cursor_pos_inner(input, cursor, area, state)
}
