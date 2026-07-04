//! Pure presentation helpers: text truncation/formatting, horizontal
//! centering, status-bar gap math, and the lightweight JSON highlighter.
//! Nothing here touches `AppState` or draws to a frame.

use crate::config::Config;
use crate::tui::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

pub(super) fn format_date(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        dt.format("%b %d %H:%M").to_string()
    } else {
        rfc3339.chars().take(16).collect()
    }
}

pub(super) fn centered_h(area: Rect, width: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    Rect::new(x, area.y, width.min(area.width), area.height)
}

/// Padding between the left/center/right segments of the status bar, sized
/// by display width (not byte length) so multibyte content — Cyrillic
/// status text, emoji in the goal/MCP tags — doesn't overstate how much
/// horizontal space a segment occupies and push `right` off-screen.
pub(super) fn status_bar_gap(left: &str, center: &str, right: &str, width: u16) -> usize {
    let segments_width = UnicodeWidthStr::width(left) + UnicodeWidthStr::width(center) + UnicodeWidthStr::width(right);
    width.saturating_sub(segments_width as u16).max(1) as usize
}

pub(super) fn provider_label(config: &Config) -> String {
    if let Some(entry) = config.active_provider_entry() {
        return entry.name.clone();
    }
    match config.provider.kind {
        crate::config::ProviderKind::Deepseek => "DeepSeek",
        crate::config::ProviderKind::Openai => "OpenAI",
        crate::config::ProviderKind::Custom => "Custom",
    }
    .to_string()
}

pub(super) fn highlight_json(text: &str, theme: &Theme) -> Vec<Line<'static>> {
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
