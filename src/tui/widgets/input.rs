use crate::app::AppState;
use crate::tui::theme::Theme;
use crate::util::char_to_byte_pos;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::cell::Cell;
use unicode_width::UnicodeWidthStr;

/// Display width of the input's left gutter: the `" >  "` prompt on the first
/// visual line, matched by a blank indent on wrapped and continuation lines so
/// every line's text starts in the same column.
const GUTTER: usize = 4;
/// Columns kept clear at the right edge so wrapped text never touches — and
/// the cursor never lands one past — the input box's last column.
const RIGHT_MARGIN: usize = 1;

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
            visual.push(VisualLine {
                logical_idx: li,
                char_start: 0,
                char_end: 0,
            });
            continue;
        }
        let mut offset = 0;
        while offset < chars.len() {
            // Uniform gutter for every visual line (first, wrapped, and new
            // logical lines) plus a right margin, so wrapping matches the
            // rendered prefixes and text never fills the last column.
            let max_width = (area_width as usize).saturating_sub(GUTTER + RIGHT_MARGIN);
            if max_width == 0 {
                let end = chars.len();
                visual.push(VisualLine {
                    logical_idx: li,
                    char_start: offset,
                    char_end: end,
                });
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
            visual.push(VisualLine {
                logical_idx: li,
                char_start: offset,
                char_end: end,
            });
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

/// Index of the first visual line to render (the scroll offset) for an input of
/// `total_rows` visual lines shown in `visible_rows` cells with `top_pad` blank
/// rows above the content. Once the input is taller than the content area it
/// scrolls to keep `cursor_visual` on the last content row, so newly typed or
/// wrapped lines stay visible instead of being drawn past the bottom of the box.
fn scroll_top_row(
    cursor_visual: usize,
    total_rows: usize,
    visible_rows: usize,
    top_pad: usize,
) -> usize {
    let content_rows = visible_rows.saturating_sub(top_pad);
    if total_rows <= content_rows {
        0
    } else {
        cursor_visual
            .saturating_sub(visible_rows.saturating_sub(1 + top_pad))
            .min(total_rows.saturating_sub(content_rows))
    }
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
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        (char_to_byte_pos(input, start), char_to_byte_pos(input, end))
    });

    let logical: Vec<&str> = input.split('\n').collect();
    let line_starts = line_starts_from_logical(&logical);
    let visual = build_visual_lines(input, area.width);

    let cursor_visual = visual
        .iter()
        .rposition(|v| {
            let log_start = line_starts[v.logical_idx];
            let abs_end = log_start + (v.char_end - v.char_start);
            cursor >= log_start + v.char_start && cursor <= abs_end
        })
        .unwrap_or(0);

    let visible_rows = area.height as usize;
    let total_rows = visual.len();
    let top_row = scroll_top_row(cursor_visual, total_rows, visible_rows, top_pad);

    let mut rendering_lines: Vec<Line> = Vec::with_capacity(visible_rows);
    let indent_width = GUTTER;
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
                rendering_lines.push(Line::from(vec![Span::styled(&blank, empty_style)]));
            }
        }
    } else {
        for _ri in 0..top_pad.min(visible_rows) {
            rendering_lines.push(Line::from(vec![Span::styled(&blank, empty_style)]));
        }
        // Render the scroll window [top_row, top_row + max_content): the visual
        // lines that fit in the content rows below the top pad. The previous
        // filter folded `top_pad` into the lower-bound test, so it failed to
        // drop the scrolled-off lines and `.take()` then clipped the cursor's
        // own line off the bottom — typing past the last visible row moved the
        // caret but rendered no text.
        let max_content = visible_rows.saturating_sub(top_pad);
        for vi in (top_row..visual.len()).take(max_content) {
            let vline = &visual[vi];
            let seg = logical[vline.logical_idx];

            let fg = Style::default().fg(theme.fg).bg(theme.input_bg);
            let sel_style = Style::default()
                .fg(theme.fg)
                .bg(theme.selection)
                .add_modifier(Modifier::REVERSED);

            let mut spans: Vec<Span> = Vec::new();
            if vline.logical_idx == 0 && vline.char_start == 0 {
                spans.push(Span::styled(
                    " >  ",
                    Style::default().fg(theme.accent).bg(theme.input_bg),
                ));
            } else {
                // GUTTER (4) blank cells, matching the `" >  "` prompt width so
                // wrapped and continuation lines align under the first line.
                spans.push(Span::styled("    ", empty_style));
            }

            let seg_byte_start = char_to_byte_pos(input, line_starts[vline.logical_idx]);
            let byte_chunk_start = char_to_byte_pos(seg, vline.char_start);
            let byte_chunk_end = char_to_byte_pos(seg, vline.char_end);
            let mut idx = byte_chunk_start;
            while idx < byte_chunk_end.min(seg.len()) {
                let Some(ch) = seg[idx..].chars().next() else {
                    break;
                };
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
                    let Some(nch) = seg[j..].chars().next() else {
                        break;
                    };
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
            rendering_lines.push(Line::from(vec![Span::styled(&blank, empty_style)]));
        }
    }

    let paragraph = Paragraph::new(rendering_lines).style(Style::default().bg(theme.input_bg));
    frame.render_widget(paragraph, area);

    let pos = cursor_pos_inner(input, cursor, area, state);
    if let Some((x, y)) = pos {
        frame.set_cursor_position((x, y));
        cursor_out.set(Some((x, y)));
    } else {
        cursor_out.set(None);
    }
}

fn cursor_pos_inner(
    input: &str,
    cursor: usize,
    area: Rect,
    state: &AppState,
) -> Option<(u16, u16)> {
    if state.focused().generation.active || state.modal.is_some() {
        return None;
    }
    let top_pad = 1;
    let logical: Vec<&str> = input.split('\n').collect();
    let line_starts = line_starts_from_logical(&logical);
    let visual = build_visual_lines(input, area.width);

    let cursor_visual = visual
        .iter()
        .rposition(|v| {
            let log_start = line_starts.get(v.logical_idx).copied().unwrap_or(0);
            let abs_end = log_start + (v.char_end - v.char_start);
            let abs_start = log_start + v.char_start;
            cursor >= abs_start && cursor <= abs_end
        })
        .unwrap_or(0);
    let cursor_logical = visual[cursor_visual].logical_idx;
    let cursor_col_in_logical = cursor - line_starts[cursor_logical];

    let visible_rows = area.height as usize;
    let total_rows = visual.len();
    let top_row = scroll_top_row(cursor_visual, total_rows, visible_rows, top_pad);
    let screen_row = top_pad + cursor_visual.saturating_sub(top_row);
    let indent = GUTTER;

    let seg = logical[cursor_logical];
    let vchar_start = visual[cursor_visual].char_start;
    let before_cursor_chars: String = seg
        .chars()
        .skip(vchar_start)
        .take(cursor_col_in_logical - vchar_start)
        .collect();
    let h_offset = u16::try_from(UnicodeWidthStr::width(before_cursor_chars.as_str()))
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub((GUTTER + RIGHT_MARGIN) as u16));
    let x = area.x + indent as u16 + h_offset;
    let cursor_y = area.y.saturating_add(screen_row as u16);
    let cursor_y = cursor_y.clamp(area.y, area.bottom().saturating_sub(1));
    Some((x, cursor_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_uses_uniform_gutter_and_right_margin() {
        // Every visual line (first, wrapped, and post-newline) wraps at the
        // same width, so wrapped text aligns under the first line and never
        // fills the last column.
        let width = 20u16;
        let expected = (width as usize) - GUTTER - RIGHT_MARGIN; // content cells per line
        let input = "a".repeat(100);
        let visual = build_visual_lines(&input, width);
        for v in &visual {
            assert!(
                v.char_end - v.char_start <= expected,
                "visual line holds {} cols, budget is {expected}",
                v.char_end - v.char_start
            );
        }
    }

    #[test]
    fn scroll_top_row_keeps_cursor_line_visible() {
        // The input box is 4 rows tall with one pad row, i.e. 3 content rows.
        let visible_rows = 4;
        let top_pad = 1;
        let content_rows = visible_rows - top_pad;
        for total_rows in 1..=30usize {
            for cursor_visual in 0..total_rows {
                let top = scroll_top_row(cursor_visual, total_rows, visible_rows, top_pad);
                // The cursor's visual line must land inside the rendered window
                // [top, top + content_rows) — the exact guarantee the old
                // filter broke, dropping the cursor's line off the bottom.
                assert!(
                    cursor_visual >= top && cursor_visual < top + content_rows,
                    "cursor {cursor_visual} outside window: total={total_rows} top={top}"
                );
                // The window never scrolls past the content into blank space.
                assert!(
                    top + content_rows <= total_rows.max(content_rows),
                    "window runs past content: total={total_rows} top={top}"
                );
                // The caret's screen row stays within the drawable rows.
                let screen_row = top_pad + cursor_visual - top;
                assert!(
                    screen_row < visible_rows,
                    "screen_row {screen_row} overflows box"
                );
            }
        }
    }

    #[test]
    fn no_scroll_until_content_exceeds_rows() {
        // Three or fewer visual lines fit without scrolling; the fourth line
        // (a third input line wrapping) starts the scroll.
        assert_eq!(scroll_top_row(2, 3, 4, 1), 0);
        assert_eq!(scroll_top_row(3, 4, 4, 1), 1);
        assert_eq!(scroll_top_row(9, 10, 4, 1), 7); // last line → last window
    }
}
