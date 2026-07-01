//! Chat transcript rendering.
//!
//! `render_chat` is called every frame, so the expensive parts — markdown
//! parsing plus syntect highlighting for assistant messages, and the
//! word-wrap row count used for scroll math — are cached per message rather
//! than recomputed on every redraw. See [`MSG_CACHE`] for the cache design.

use crate::app::AppState;
use crate::provider::{estimate_tokens, ChatMessage, Role};
use crate::tui::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};
use std::cell::RefCell;
use std::collections::HashMap;

/// Above this many entries the per-message cache is dropped wholesale
/// instead of tracking per-entry LRU — simple, and cheap enough since a
/// clear just means the next frame repopulates from (mostly) cache-warm
/// content again.
const MAX_CACHE_ENTRIES: usize = 4096;

/// Identifies one message's rendered content for caching purposes.
///
/// `content_len` (byte length of `visible_content()`) is monotonically
/// increasing while a message streams, so the cache key changes every time
/// new tokens arrive — the streaming tail always misses — but a finished
/// message's key is stable forever, so it hits on every subsequent frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    conversation_id: u64,
    message_index: usize,
    content_len: usize,
    wrap_width: u16,
}

/// Cached render output for one assistant message at one wrap width.
///
/// Token counts are *not* stored here — they don't depend on `wrap_width`,
/// so they live in the separate [`TOKEN_CACHE`] shared with the stats panel.
struct CachedMsg {
    /// Rendered markdown, owned so the cache can outlive the frame that
    /// produced it (spans hold `String`s, not borrows into `msg.content`).
    lines: Vec<Line<'static>>,
    /// Word-wrapped row count of `lines` at the key's `wrap_width`, computed
    /// once via `Paragraph::line_count` instead of re-wrapping every frame.
    wrapped_rows: usize,
    /// Cheap fingerprint of the content this entry was built from. Messages
    /// can be popped (e.g. an empty assistant reply discarded mid-stream),
    /// which can shift a later message into an earlier one's `(index, len)`
    /// slot; comparing the fingerprint on hit catches that collision instead
    /// of serving stale spans for different content.
    fingerprint: u64,
}

/// `(conversation_id, message_index, content_len)` — like [`CacheKey`] but
/// without `wrap_width`, since a token count doesn't depend on the viewport.
type TokenCacheKey = (u64, usize, usize);
/// `(tokens, fingerprint)` — the fingerprint guards against a `TokenCacheKey`
/// collision serving a stale count for different content, same as
/// [`CachedMsg::fingerprint`].
type TokenCacheValue = (u32, u64);

thread_local! {
    static MSG_CACHE: RefCell<HashMap<CacheKey, CachedMsg>> = RefCell::new(HashMap::new());
    /// Token estimates keyed without `wrap_width` (unlike [`MSG_CACHE`]),
    /// since a token count doesn't depend on the viewport. Shared with the
    /// stats panel (`widgets::panel::compute_totals`) so it doesn't need to
    /// know the chat viewport's wrap width just to look up a token count.
    static TOKEN_CACHE: RefCell<HashMap<TokenCacheKey, TokenCacheValue>> = RefCell::new(HashMap::new());
}

/// Resolved token count for message `index` in conversation `conversation_id`,
/// cached across frames and shared between the chat transcript and the stats
/// panel. Falls back to computing directly (and populating the cache) on a
/// miss, so callers don't need `msg` to have already gone through
/// [`render_assistant_cached`].
pub(crate) fn cached_token_estimate(conversation_id: u64, message_index: usize, msg: &ChatMessage) -> u32 {
    let content = msg.visible_content();
    let key = (conversation_id, message_index, content.len());
    let fp = fingerprint(content);

    TOKEN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((tokens, cached_fp)) = cache.get(&key)
            && *cached_fp == fp {
                return *tokens;
            }
        if cache.len() > MAX_CACHE_ENTRIES {
            cache.clear();
        }
        let tokens = msg.total_tokens.unwrap_or_else(|| estimate_tokens(&msg.content));
        cache.insert(key, (tokens, fp));
        tokens
    })
}

/// Fold the first and last 8 bytes of `content` into a `u64`. Not
/// cryptographic — just cheap insurance against an `(index, len, width)` key
/// collision after messages are popped from the transcript (see
/// [`CachedMsg::fingerprint`]).
fn fingerprint(content: &str) -> u64 {
    let bytes = content.as_bytes();
    let mut buf = [0u8; 16];
    let head_len = bytes.len().min(8);
    buf[..head_len].copy_from_slice(&bytes[..head_len]);
    if bytes.len() > 8 {
        let tail_len = bytes.len().min(16) - 8;
        buf[8..8 + tail_len].copy_from_slice(&bytes[bytes.len() - tail_len..]);
    }
    let (head, tail) = buf.split_at(8);
    let head = u64::from_le_bytes(head.try_into().unwrap());
    let tail = u64::from_le_bytes(tail.try_into().unwrap());
    head.rotate_left(1) ^ tail.wrapping_mul(0x9E3779B97F4A7C15) ^ (bytes.len() as u64)
}

/// Word-wrapped row count for `lines` at `width`, using ratatui's real
/// `WordWrapper` (via `Paragraph::line_count`) rather than an approximation
/// — this is what `Paragraph::wrap(Wrap { trim: false })` will actually
/// produce, including greedy fill by display width, breaking at whitespace,
/// force-breaking overlong words, and `trim: false`'s preserved indentation.
///
/// Takes ownership of `lines` (`Paragraph`/`Text` own their line buffer);
/// generic over the borrow lifetime so callers with borrowed spans (e.g. a
/// `Role::System` line built straight from `msg.visible_content()`) don't
/// need to eagerly copy into `'static` strings just to count rows.
fn wrapped_row_count(lines: Vec<Line<'_>>, width: usize) -> usize {
    if width == 0 {
        return lines.len().max(1);
    }
    let count = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width as u16);
    count.max(1)
}

fn format_time(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        dt.format("%H:%M:%S").to_string()
    } else {
        rfc3339.chars().take(8).collect()
    }
}

fn meta_line(msg: &ChatMessage, tokens: u32, theme: &Theme) -> Line<'static> {
    let time = format_time(&msg.created_at);
    let mut parts = vec![format!(" {}  {}t ", time, tokens)];
    if msg.think_elapsed_secs > 0.0 {
        parts.push(format!("think {:.1}s", msg.think_elapsed_secs));
    }
    if msg.references_count > 0 {
        parts.push(format!("\u{1F4CE}{}", msg.references_count));
    }
    let meta = parts.join("  ");
    Line::from(vec![Span::styled(
        meta,
        Style::default().fg(theme.text_dim).bg(theme.bg),
    )])
}

/// Render (or reuse the cached render of) one assistant message's markdown,
/// its word-wrapped row count, and its resolved token count.
///
/// Returns owned data (not a reference into the cache) so the caller can
/// extend its line buffer without fighting the `RefCell` borrow.
fn render_assistant_cached(
    conversation_id: u64,
    message_index: usize,
    msg: &ChatMessage,
    wrap_width: u16,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize, u32) {
    let content = msg.visible_content();
    let key = CacheKey {
        conversation_id,
        message_index,
        content_len: content.len(),
        wrap_width,
    };
    let fp = fingerprint(content);

    let token_estimate = cached_token_estimate(conversation_id, message_index, msg);

    MSG_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        if let Some(cached) = cache.get(&key)
            && cached.fingerprint == fp {
                return (cached.lines.clone(), cached.wrapped_rows, token_estimate);
            }

        if cache.len() > MAX_CACHE_ENTRIES {
            cache.clear();
        }

        let rendered_lines = {
            let mut md_lines = crate::tui::markdown::render_markdown(content, theme);
            compact_lines(&mut md_lines);
            md_lines
        };
        let wrapped_rows = wrapped_row_count(rendered_lines.clone(), wrap_width as usize);

        cache.insert(
            key,
            CachedMsg {
                lines: rendered_lines.clone(),
                wrapped_rows,
                fingerprint: fp,
            },
        );

        (rendered_lines, wrapped_rows, token_estimate)
    })
}

pub fn render_chat(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();
    let mut total_rows = 0usize;
    let conversation_id = state.conversations.focused_id().0;
    let wrap_width = area.width;

    for (index, msg) in state.focused().messages.iter().enumerate() {
        let before = lines.len();
        match msg.role {
            Role::User => {
                for line_text in msg.visible_content().lines() {
                    lines.push(Line::from(vec![Span::styled(
                        format!(" {} ", line_text),
                        Style::default().fg(theme.fg).bg(theme.user_bg),
                    )]));
                }
                let tokens = cached_token_estimate(conversation_id, index, msg);
                lines.push(meta_line(msg, tokens, theme));
            }
            Role::Assistant => {
                let header_color = match msg.status.as_deref() {
                    Some("ABORTED") => theme.error,
                    Some("WIP") => theme.warning,
                    _ => theme.success,
                };
                let model_tag = if !msg.model.is_empty() {
                    format!(" ({})", msg.model)
                } else {
                    String::new()
                };
                let header = Span::styled(
                    format!(" pooprusteek{} ", model_tag),
                    Style::default()
                        .fg(theme.bg)
                        .bg(header_color)
                        .add_modifier(Modifier::BOLD),
                );
                if msg.content.is_empty() {
                    lines.push(Line::from(vec![
                        header,
                        Span::styled(
                            " Thinking...",
                            Style::default().fg(theme.text_dim),
                        ),
                    ]));
                } else {
                    let (md_lines, md_rows, tokens) =
                        render_assistant_cached(conversation_id, index, msg, wrap_width, theme);
                    lines.push(Line::from(vec![header]));
                    total_rows += 1;
                    lines.extend(md_lines);
                    total_rows += md_rows;
                    let meta = meta_line(msg, tokens, theme);
                    total_rows += wrapped_row_count(vec![meta.clone()], wrap_width as usize);
                    lines.push(meta);
                    lines.push(Line::from(""));
                    total_rows += 1;
                    continue;
                }
            }
            Role::System => {
                lines.push(Line::from(vec![
                    Span::styled(
                        " \u{2139} ",
                        Style::default().fg(theme.warning).bg(theme.bg),
                    ),
                    Span::styled(
                        msg.visible_content(),
                        Style::default().fg(theme.text_dim).bg(theme.bg),
                    ),
                ]));
            }
            Role::Tool => {
                let tool_name = msg.name.as_deref().unwrap_or("unknown");
                let icon = if msg.tool_error { "\u{2717}" } else { "\u{2713}" };
                let status_color = if msg.tool_error { theme.error } else { theme.success };

                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {icon} "),
                        Style::default()
                            .fg(theme.bg)
                            .bg(status_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {tool_name} "),
                        Style::default()
                            .fg(theme.bg)
                            .bg(if msg.tool_error { theme.error } else { theme.accent })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if msg.tool_error { " failed " } else { " done " },
                        Style::default().fg(theme.text_dim).bg(theme.tool_bg),
                    ),
                ]));

                let body = msg.visible_content();
                let body_lines: Vec<&str> = body.lines().collect();
                if body_lines.iter().all(|l| l.trim().is_empty()) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  (no output)",
                            Style::default().fg(theme.text_dim).bg(theme.tool_bg),
                        ),
                    ]));
                } else {
                    for line_text in &body_lines {
                        lines.push(Line::from(vec![
                            Span::styled(
                                " \u{2502} ",
                                Style::default().fg(theme.accent_soft).bg(theme.tool_bg),
                            ),
                            Span::styled(
                                (*line_text).to_string(),
                                Style::default()
                                    .fg(if msg.tool_error { theme.error } else { theme.text_soft })
                                    .bg(theme.tool_bg),
                            ),
                        ]));
                    }
                }
            }
        }
        // Non-assistant (or empty-assistant) segment: word-wrap row count is
        // computed fresh each frame. These bubbles are short (a handful of
        // lines) compared to assistant markdown blocks, so this isn't the
        // per-frame cost fix 1/6 target — the cache above is. A structural
        // clone (not a re-parse) is enough since `Paragraph` needs to own
        // its line buffer.
        let segment: Vec<Line> = lines[before..].to_vec();
        total_rows += wrapped_row_count(segment, wrap_width as usize);
        lines.push(Line::from(""));
        total_rows += 1;
    }

    let visible_height = area.height as usize;
    let max_scroll = total_rows.saturating_sub(visible_height);
    let scroll_from_bottom = (state.scroll_offset as usize).min(max_scroll);
    let top_row = max_scroll.saturating_sub(scroll_from_bottom);

    let paragraph = Paragraph::new(lines)
        .style(Style::default().bg(theme.bg))
        .wrap(Wrap { trim: false })
        .scroll((top_row as u16, 0));
    frame.render_widget(paragraph, area);
}

fn is_blank(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn compact_lines(lines: &mut Vec<Line<'static>>) {
    // Remove leading blank lines
    while lines.first().is_some_and(|l| is_blank(l)) {
        lines.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_differs_for_different_content() {
        assert_ne!(fingerprint("hello world"), fingerprint("hello there"));
    }

    #[test]
    fn fingerprint_stable_for_same_content() {
        let a = fingerprint("the quick brown fox jumps over the lazy dog");
        let b = fingerprint("the quick brown fox jumps over the lazy dog");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_handles_short_content() {
        // Content shorter than 8 or 16 bytes must not panic (head/tail overlap).
        let _ = fingerprint("");
        let _ = fingerprint("a");
        let _ = fingerprint("1234567");
        let _ = fingerprint("12345678");
        let _ = fingerprint("123456789");
    }

    #[test]
    fn fingerprint_sensitive_to_length_with_same_head_tail() {
        // Same first/last 8 bytes, different length in between — length term
        // in the fold should still distinguish them in the common case.
        let a = fingerprint("AAAAAAAA_paddingA_BBBBBBBB");
        let b = fingerprint("AAAAAAAA_BBBBBBBB");
        assert_ne!(a, b);
    }

    #[test]
    fn wrapped_row_count_plain_ascii_exact_fit() {
        let lines = vec![Line::from("aaaaaaaaaa")]; // exactly 10 chars
        assert_eq!(wrapped_row_count(lines, 10), 1);
    }

    #[test]
    fn wrapped_row_count_long_word_force_breaks() {
        // A single word longer than the width must still produce >1 row.
        let lines = vec![Line::from("supercalifragilisticexpialidocious")];
        let rows = wrapped_row_count(lines, 10);
        assert!(rows > 1, "expected long word to force-break, got {rows} rows");
    }

    #[test]
    fn wrapped_row_count_cjk_double_width() {
        // 10 CJK characters at 2 cells each = 20 display cells; at width 10
        // that's 2 rows, not 5 (a char-count estimator would say ~10/10=1).
        let lines = vec![Line::from("你好世界你好世界你好")];
        let rows = wrapped_row_count(lines, 10);
        assert_eq!(rows, 2);
    }

    #[test]
    fn wrapped_row_count_width_10_three_words_three_rows() {
        // "aaaaaa bbbbbb cccccc" at width 10: greedy word-wrap puts one word
        // per row since no two words fit together in 10 cells.
        let lines = vec![Line::from("aaaaaa bbbbbb cccccc")];
        assert_eq!(wrapped_row_count(lines, 10), 3);
    }

    #[test]
    fn wrapped_row_count_zero_width_falls_back_to_line_count() {
        let lines = vec![Line::from("a"), Line::from("b")];
        assert_eq!(wrapped_row_count(lines, 0), 2);
    }

    #[test]
    fn cached_token_estimate_uses_total_tokens_when_present() {
        let mut msg = ChatMessage::assistant("hello world");
        msg.total_tokens = Some(42);
        assert_eq!(cached_token_estimate(999_001, 0, &msg), 42);
    }

    #[test]
    fn cached_token_estimate_falls_back_to_estimate_when_absent() {
        let msg = ChatMessage::user("hello world");
        assert_eq!(cached_token_estimate(999_002, 0, &msg), estimate_tokens(&msg.content));
    }

    #[test]
    fn cached_token_estimate_reflects_updated_content_after_streaming() {
        // Same (conversation, index) but content grew — the cache key
        // includes content_len, so this must not return the stale value.
        let short = ChatMessage::assistant("short");
        let short_tokens = cached_token_estimate(999_003, 0, &short);

        let long = ChatMessage::assistant("a much, much longer streamed response body");
        let long_tokens = cached_token_estimate(999_003, 0, &long);

        assert_ne!(short_tokens, long_tokens);
        assert_eq!(long_tokens, estimate_tokens(&long.content));
    }
}
