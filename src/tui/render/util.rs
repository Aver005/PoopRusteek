//! Pure presentation helpers: text truncation/formatting, horizontal
//! centering, status-bar gap math, and the lightweight JSON highlighter.
//! Nothing here touches `AppState` or draws to a frame.

use crate::config::Config;
use crate::tui::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Truncate `s` to at most `max` display columns, appending `…` when cut.
/// Measures display cells like [`fit_col`] — a char count under-measures
/// wide glyphs (CJK, emoji) and misaligns the fixed-width layouts these
/// strings land in next to dates, badges, and checkboxes.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        // Нулевая ширина: многоточие шириной в одну клетку вышло бы за предел.
        return String::new();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Fit `s` into exactly `width` display columns for fixed-column table layout:
/// truncate with a trailing `…` when too wide, right-pad with spaces when too
/// narrow. Width is measured in display cells (via `UnicodeWidthStr`), not
/// bytes or chars, so Cyrillic, CJK, and emoji content stays column-aligned.
pub(super) fn fit_col(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w == width {
        return s.to_string();
    }
    if w < width {
        let mut out = String::with_capacity(s.len() + (width - w));
        out.push_str(s);
        out.push_str(&" ".repeat(width - w));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    // Too wide: take as many chars as fit in `width - 1` cells (reserving one
    // for the ellipsis), then pad in case a wide char left a one-cell gap.
    let budget = width - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    used += 1;
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

/// Короткая дата для списков: время, если это `today`, иначе `Nov 05`.
pub(super) fn short_date(rfc3339: &str, today: chrono::NaiveDate) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return truncate(rfc3339, SHORT_DATE_COLS);
    };
    let local = dt.with_timezone(&chrono::Local);
    if local.date_naive() == today {
        local.format("%H:%M").to_string()
    } else {
        local.format("%b %d").to_string()
    }
}

/// Ширина [`short_date`]: `Nov 05` и `14:02` влезают.
pub(super) const SHORT_DATE_COLS: usize = 6;

/// Сколько клеток левой части остаётся при любой длине правой.
const MIN_LEFT_COLS: usize = 10;

/// Части по порядку, урезанные до `max` клеток: режется первая не влезшая, с `…`.
fn fit_spans<'a>(spans: Vec<Span<'a>>, max: usize) -> (Vec<Span<'a>>, usize) {
    let mut used = 0;
    let mut out = Vec::new();
    for span in spans {
        let width = UnicodeWidthStr::width(span.content.as_ref());
        if used + width <= max {
            used += width;
            out.push(span);
            continue;
        }
        let cut = truncate(span.content.as_ref(), max - used);
        used += UnicodeWidthStr::width(cut.as_str());
        out.push(Span::styled(cut, span.style));
        break;
    }
    (out, used)
}

/// Строка ровно в `width` клеток: слева суть, справа метка. Правая часть не
/// отнимает у левой больше, чем оставляет ей [`MIN_LEFT_COLS`].
pub(super) fn split_line<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: usize) -> Line<'a> {
    let (right, right_w) = fit_spans(right, width.saturating_sub(MIN_LEFT_COLS + 1));
    let gap = usize::from(right_w > 0);
    let (mut spans, left_w) = fit_spans(left, width.saturating_sub(right_w + gap));
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(left_w + right_w)),
    ));
    spans.extend(right);
    Line::from(spans)
}

pub(super) fn provider_label(config: &Config) -> String {
    config.active_provider_name()
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
            // Theme role, not a literal — the one hardcoded RGB in render/
            // violated the "colors come from Theme" rule.
            Style::default().fg(theme.warning)
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

                let mut spans = vec![Span::styled(
                    indent.to_string(),
                    Style::default().fg(theme.fg),
                )];
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

    fn cells(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    }

    #[test]
    fn a_short_date_is_a_time_today_and_a_day_otherwise() {
        let stamp = "2026-11-05T09:12:00+00:00";
        let local = chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(
            short_date(stamp, local.date_naive()),
            local.format("%H:%M").to_string()
        );
        let other = local.date_naive().succ_opt().unwrap();
        assert_eq!(short_date(stamp, other), local.format("%b %d").to_string());
        assert_eq!(
            UnicodeWidthStr::width(short_date("испорченная дата", other).as_str()),
            6
        );
    }

    #[test]
    fn a_split_line_is_exactly_as_wide_as_asked() {
        for width in 0..40 {
            let line = split_line(
                vec![Span::raw("claude-max"), Span::raw(" · подписка")],
                vec![Span::raw("oauth token expired, run /accounts auth")],
                width,
            );
            assert_eq!(cells(&line), width, "width {width}");
        }
    }

    #[test]
    fn a_long_right_side_never_eats_the_whole_left() {
        let line = split_line(
            vec![Span::raw("claude-max")],
            vec![Span::raw("oauth token expired, run /accounts auth")],
            29,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("claude-max"), "{text}");
        assert!(text.contains('…'), "{text}");
    }

    #[test]
    fn truncate_to_zero_width_gives_nothing_not_an_ellipsis() {
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("", 0), "");
        assert_eq!(truncate("abc", 1), "\u{2026}");
    }

    #[test]
    fn fit_col_pads_short_and_truncates_long() {
        // Short content is right-padded to the exact column width.
        assert_eq!(fit_col("ab", 5), "ab   ");
        // Exact-width content is returned unchanged.
        assert_eq!(fit_col("abcde", 5), "abcde");
        // Over-width content is truncated with a trailing ellipsis; the result
        // is still exactly `width` display columns.
        let out = fit_col("abcdefgh", 5);
        assert_eq!(out, "abcd…");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
    }

    #[test]
    fn fit_col_multibyte_uses_display_width() {
        // Cyrillic letters are 1 cell each.
        assert_eq!(fit_col("привет", 4), "при…");
        // CJK chars are 2 cells each: "世界世" is 6 cells; fitting to 5 keeps
        // two chars + … = 5 cells exactly (not 5 chars).
        let out = fit_col("世界世", 5);
        assert_eq!(out, "世界…");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
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
        assert_eq!(
            first_span.content.as_ref(),
            "  ",
            "indent span must be exactly the 2 leading spaces"
        );

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
