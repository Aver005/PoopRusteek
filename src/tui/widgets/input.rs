use crate::app::AppState;
use crate::tui::render::MAX_INPUT_ROWS;
use crate::tui::theme::Theme;
use crate::util::char_to_byte_pos;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

/// Display width of the input's left gutter: the `" ❯ "` prompt on the first
/// visual line, matched by a blank indent on wrapped and continuation lines so
/// every line's text starts in the same column.
const GUTTER: usize = 3;
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

/// Сколько строк нужно полю ввода при ширине `width`: от одной до предела.
pub fn visible_rows(buffer: &str, width: u16) -> u16 {
    let rows = build_visual_lines(buffer, width).len() as u16;
    rows.clamp(1, MAX_INPUT_ROWS)
}

/// Подсказка в пустом поле: зависит от того, что сейчас происходит.
pub fn placeholder(state: &AppState) -> &'static str {
    let focused = state.focused();
    if state.goal.is_setup_pending() {
        use crate::app::events::GoalStage;
        return if state.goal.stage == GoalStage::WaitForGoal {
            "Your next message becomes the goal \u{2014} Esc cancels GOAL setup"
        } else {
            "GOAL setup: describe the task, then the goal \u{2014} Esc cancels"
        };
    }
    if focused.generation.active {
        if focused.queued.is_empty() {
            "Type a follow-up \u{2014} Enter queues it until the turn ends\u{2026}"
        } else {
            "Add another message to the queue\u{2026}"
        }
    } else if focused.messages.is_empty() {
        "Ask anything \u{2014} describe a task, debug an issue, or ask a question\u{2026}"
    } else {
        "Type a message or / for commands\u{2026}"
    }
}

/// Раскладка текста по строкам, посчитанная один раз за кадр.
struct InputLayout<'a> {
    logical: Vec<&'a str>,
    line_starts: Vec<usize>,
    visual: Vec<VisualLine>,
}

impl<'a> InputLayout<'a> {
    fn new(input: &'a str, width: u16) -> Self {
        let logical: Vec<&str> = input.split('\n').collect();
        let line_starts = line_starts_from_logical(&logical);
        let visual = build_visual_lines(input, width);
        Self {
            logical,
            line_starts,
            visual,
        }
    }

    /// Номер визуальной строки, в которой стоит курсор.
    fn cursor_row(&self, cursor: usize) -> usize {
        self.visual
            .iter()
            .rposition(|v| {
                let log_start = self.line_starts.get(v.logical_idx).copied().unwrap_or(0);
                let start = log_start + v.char_start;
                let end = log_start + v.char_end;
                cursor >= start && cursor <= end
            })
            .unwrap_or(0)
    }
}

/// Нарисовать поле ввода в `area` (одна или несколько строк на всю ширину) и
/// вернуть, где стоит курсор. Курсор ставит вызывающий.
pub fn render_input(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
) -> Option<(u16, u16)> {
    let input = &state.input.buffer;
    let cursor = state.input.cursor.min(input.chars().count());
    let layout = InputLayout::new(input, area.width);
    let visible_rows = area.height as usize;
    let cursor_row = layout.cursor_row(cursor);
    let top_row = scroll_top_row(cursor_row, layout.visual.len(), visible_rows, 0);
    let glyph = crate::tui::glyphs::get();

    let base = Style::default().fg(theme.fg).bg(theme.input_bg);
    let blank = Style::default().bg(theme.input_bg);
    let prompt = Span::styled(
        format!(" {} ", glyph.prompt),
        Style::default().fg(theme.accent).bg(theme.input_bg),
    );
    let indent = Span::styled(" ".repeat(GUTTER), blank);

    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows);
    if input.is_empty() {
        lines.push(Line::from(vec![
            prompt,
            Span::styled(
                placeholder(state),
                Style::default().fg(theme.text_dim).bg(theme.input_bg),
            ),
        ]));
    } else {
        let sel: Option<(usize, usize)> = state.input.selection_anchor.map(|anchor| {
            let (start, end) = if anchor <= cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            (char_to_byte_pos(input, start), char_to_byte_pos(input, end))
        });
        let sel_style = Style::default().fg(theme.fg).bg(theme.selection);
        for vi in (top_row..layout.visual.len()).take(visible_rows) {
            let vline = &layout.visual[vi];
            let seg = layout.logical[vline.logical_idx];
            let mut spans: Vec<Span> = Vec::new();
            let first_line = vline.logical_idx == 0 && vline.char_start == 0;
            spans.push(if first_line {
                prompt.clone()
            } else {
                indent.clone()
            });

            let seg_byte_start = char_to_byte_pos(input, layout.line_starts[vline.logical_idx]);
            let chunk_start = char_to_byte_pos(seg, vline.char_start);
            let chunk_end = char_to_byte_pos(seg, vline.char_end).min(seg.len());
            let in_selection = |byte: usize| sel.is_some_and(|(s, e)| byte >= s && byte < e);
            let mut idx = chunk_start;
            while idx < chunk_end {
                let selected = in_selection(seg_byte_start + idx);
                let mut next = idx;
                while next < chunk_end && in_selection(seg_byte_start + next) == selected {
                    let Some(ch) = seg[next..].chars().next() else {
                        break;
                    };
                    next += ch.len_utf8();
                }
                if next == idx {
                    break;
                }
                spans.push(Span::styled(
                    &seg[idx..next],
                    if selected { sel_style } else { base },
                ));
                idx = next;
            }
            if idx == chunk_start {
                spans.push(Span::styled("", base));
            }
            lines.push(Line::from(spans));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.input_bg)),
        area,
    );
    cursor_position(&layout, cursor, cursor_row, top_row, area)
}

/// Клетка курсора: после последнего знака перед ним в его строке.
fn cursor_position(
    layout: &InputLayout<'_>,
    cursor: usize,
    cursor_row: usize,
    top_row: usize,
    area: Rect,
) -> Option<(u16, u16)> {
    let vline = layout.visual.get(cursor_row)?;
    let line_start = layout.line_starts[vline.logical_idx];
    let seg = layout.logical[vline.logical_idx];
    let before: String = seg
        .chars()
        .skip(vline.char_start)
        .take((cursor - line_start).saturating_sub(vline.char_start))
        .collect();
    let h_offset = u16::try_from(UnicodeWidthStr::width(before.as_str()))
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub((GUTTER + RIGHT_MARGIN) as u16));
    let x = area.x + GUTTER as u16 + h_offset;
    let row = cursor_row.saturating_sub(top_row) as u16;
    let y = (area.y + row).clamp(area.y, area.bottom().saturating_sub(1));
    Some((x, y))
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

    #[test]
    fn the_input_is_one_row_when_empty_and_grows_to_the_limit() {
        assert_eq!(visible_rows("", 80), 1);
        assert_eq!(visible_rows("one line", 80), 1);
        assert_eq!(visible_rows("a\nb\nc", 80), 3);
        assert_eq!(visible_rows(&"x\n".repeat(50), 80), MAX_INPUT_ROWS);
    }

    #[test]
    fn the_placeholder_follows_what_is_going_on() {
        let mut state = AppState::new(crate::app::conversation::Conversation::fresh_main(None));
        let landing = placeholder(&state);
        state
            .focused_mut()
            .messages
            .push(crate::provider::ChatMessage::user("hi"));
        let chat = placeholder(&state);
        state.focused_mut().generation.active = true;
        let running = placeholder(&state);
        assert!(landing != chat && chat != running && landing != running);
        assert!(running.contains("queue"));
    }

    #[test]
    fn goal_setup_says_the_next_message_becomes_the_goal() {
        use crate::app::events::GoalStage;
        let mut state = AppState::new(crate::app::conversation::Conversation::fresh_main(None));
        state.goal.activate();
        let first = placeholder(&state);
        state.goal.stage = GoalStage::WaitForGoal;
        let second = placeholder(&state);
        assert!(second.contains("becomes the goal"), "{second}");
        assert!(first.contains("GOAL setup") && first != second);
    }
}
