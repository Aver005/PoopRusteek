//! Chat transcript rendering.
//!
//! `render_chat` is called every frame, so the expensive parts — markdown
//! parsing plus syntect highlighting for assistant messages, and the
//! word-wrap row count used for scroll math — are cached per message rather
//! than recomputed on every redraw. See [`MSG_CACHE`] for the cache design.

use crate::app::AppState;
use crate::provider::{ChatMessage, Role, estimate_tokens};
use crate::tui::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
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
    /// Word-wrapped row count of the message's meta line at the same width —
    /// needed for scroll math even when the message is culled off-screen.
    meta_rows: usize,
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
    /// Wrapped row counts (value: `(fingerprint, rows)`) for non-assistant
    /// bubbles — user/system/tool messages and the empty-assistant
    /// "Thinking..." placeholder. Row counts don't depend on the theme
    /// (styles never change wrapping), so entries survive theme switches.
    /// Lets `render_chat`'s counting pass skip re-word-wrapping every
    /// bubble on every frame, which together with viewport culling is what
    /// keeps typing latency flat on long transcripts.
    static ROWS_CACHE: RefCell<HashMap<CacheKey, (u64, usize)>> = RefCell::new(HashMap::new());
}

/// Resolved token count for message `index` in conversation `conversation_id`,
/// cached across frames and shared between the chat transcript and the stats
/// panel. Falls back to computing directly (and populating the cache) on a
/// miss, so callers don't need `msg` to have already gone through
/// [`render_assistant_cached`].
pub(crate) fn cached_token_estimate(
    conversation_id: u64,
    message_index: usize,
    msg: &ChatMessage,
) -> u32 {
    let content = msg.visible_content();
    let key = (conversation_id, message_index, content.len());
    let fp = fingerprint(content);

    TOKEN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((tokens, cached_fp)) = cache.get(&key)
            && *cached_fp == fp
        {
            return *tokens;
        }
        if cache.len() > MAX_CACHE_ENTRIES {
            cache.clear();
        }
        let tokens = msg
            .total_tokens
            .unwrap_or_else(|| estimate_tokens(&msg.content));
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

/// Run `f` against the cached render of one assistant message (markdown
/// lines + row counts), populating the cache on a miss. The closure shape
/// lets the counting pass read row counts without deep-cloning the cached
/// lines — only messages that actually reach the screen pay the clone.
fn with_assistant_cached<R>(
    conversation_id: u64,
    message_index: usize,
    msg: &ChatMessage,
    wrap_width: u16,
    theme: &Theme,
    f: impl FnOnce(&CachedMsg, u32) -> R,
) -> R {
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
            && cached.fingerprint == fp
        {
            return f(cached, token_estimate);
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
        let meta_rows = wrapped_row_count(
            vec![meta_line(msg, token_estimate, theme)],
            wrap_width as usize,
        );

        // Plain `insert`, not `entry().or_insert()` — a fingerprint
        // mismatch above means the existing entry is stale for this slot
        // and must be replaced, not kept.
        cache.insert(
            key,
            CachedMsg {
                lines: rendered_lines,
                wrapped_rows,
                meta_rows,
                fingerprint: fp,
            },
        );
        let entry = cache.get(&key).expect("entry inserted just above");
        f(entry, token_estimate)
    })
}

/// A role discriminant folded into [`ROWS_CACHE`] fingerprints so a popped
/// slot refilled by a different-role message with coincidentally identical
/// content can't serve a stale row count.
fn role_tag(msg: &ChatMessage) -> u64 {
    match msg.role {
        Role::User => 1,
        Role::Assistant => 2,
        Role::System => 3,
        Role::Tool => 4,
    }
}

/// Build the display lines for a non-assistant bubble (user/system/tool)
/// or the empty-assistant "Thinking..." placeholder — everything except the
/// cached-markdown assistant path. Pure line construction; the caller
/// appends the shared trailing blank line.
fn build_segment_lines(
    conversation_id: u64,
    index: usize,
    msg: &ChatMessage,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
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
            // Only the empty-content placeholder goes through here; the
            // markdown path is `with_assistant_cached`.
            lines.push(Line::from(vec![
                assistant_header(msg, theme),
                Span::styled(" Thinking...", Style::default().fg(theme.text_dim)),
            ]));
        }
        Role::System => {
            let body = msg.visible_content();
            let body_lines: Vec<&str> = body.lines().collect();
            // A `Span`'s text is one terminal row — embedded `\n`s inside
            // it don't create new rows, they just get glued onto the same
            // line. Split on lines like `Role::Tool`/`Role::User` do, so
            // multi-line system messages (e.g. `/rate`'s current-settings
            // + usage) actually render as separate lines.
            if body_lines.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    " \u{2139} ",
                    Style::default().fg(theme.warning).bg(theme.bg),
                )]));
            } else {
                for (i, line_text) in body_lines.iter().enumerate() {
                    let prefix = if i == 0 { " \u{2139} " } else { "   " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(theme.warning).bg(theme.bg)),
                        Span::styled(
                            (*line_text).to_string(),
                            Style::default().fg(theme.text_dim).bg(theme.bg),
                        ),
                    ]));
                }
            }
        }
        Role::Tool => {
            let tool_name = msg.name.as_deref().unwrap_or("unknown");
            let icon = if msg.tool_error {
                "\u{2717}"
            } else {
                "\u{2713}"
            };
            let status_color = if msg.tool_error {
                theme.error
            } else {
                theme.success
            };

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
                        .bg(if msg.tool_error {
                            theme.error
                        } else {
                            theme.accent
                        })
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
                lines.push(Line::from(vec![Span::styled(
                    "  (no output)",
                    Style::default().fg(theme.text_dim).bg(theme.tool_bg),
                )]));
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
                                .fg(if msg.tool_error {
                                    theme.error
                                } else {
                                    theme.text_soft
                                })
                                .bg(theme.tool_bg),
                        ),
                    ]));
                }
            }
        }
    }
    lines
}

/// The colored ` pooprusteek (model) ` header span of an assistant bubble.
fn assistant_header(msg: &ChatMessage, theme: &Theme) -> Span<'static> {
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
    Span::styled(
        format!(" pooprusteek{} ", model_tag),
        Style::default()
            .fg(theme.bg)
            .bg(header_color)
            .add_modifier(Modifier::BOLD),
    )
}

/// Whether `msg` renders through the cached-markdown assistant path (true)
/// or as a plain segment bubble (false). One predicate so the counting and
/// materializing passes can never disagree on a message's shape.
fn is_markdown_assistant(msg: &ChatMessage) -> bool {
    msg.role == Role::Assistant && !msg.content.is_empty()
}

/// Total wrapped rows one message occupies in the transcript, including its
/// trailing blank separator line. Row counts are cached; the row math is
/// identical to what the materializing pass produces (assistant header
/// counted as one row, exactly as before).
fn message_rows(
    conversation_id: u64,
    index: usize,
    msg: &ChatMessage,
    wrap_width: u16,
    theme: &Theme,
) -> usize {
    if is_markdown_assistant(msg) {
        return with_assistant_cached(
            conversation_id,
            index,
            msg,
            wrap_width,
            theme,
            |cached, _| 1 + cached.wrapped_rows + cached.meta_rows + 1,
        );
    }

    let content = msg.visible_content();
    let key = CacheKey {
        conversation_id,
        message_index: index,
        content_len: content.len(),
        wrap_width,
    };
    let fp = fingerprint(content) ^ (role_tag(msg) << 60);

    ROWS_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((cached_fp, rows)) = cache.get(&key)
            && *cached_fp == fp
        {
            return *rows;
        }
        if cache.len() > MAX_CACHE_ENTRIES {
            cache.clear();
        }
        let segment = build_segment_lines(conversation_id, index, msg, theme);
        let rows = wrapped_row_count(segment, wrap_width as usize) + 1;
        cache.insert(key, (fp, rows));
        rows
    })
}

pub fn render_chat(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let conversation_id = state.conversations.focused_id().0;
    let wrap_width = area.width;
    let messages = &state.focused().messages;

    // Counting pass: cached wrapped row counts only — no line building, no
    // cloning — so scroll math stays exact while off-screen messages cost
    // one HashMap lookup each.
    let mut row_starts = Vec::with_capacity(messages.len());
    let mut rows_per_msg = Vec::with_capacity(messages.len());
    let mut total_rows = 0usize;
    for (index, msg) in messages.iter().enumerate() {
        let rows = message_rows(conversation_id, index, msg, wrap_width, theme);
        row_starts.push(total_rows);
        rows_per_msg.push(rows);
        total_rows += rows;
    }

    let visible_height = area.height as usize;
    let max_scroll = total_rows.saturating_sub(visible_height);
    let scroll_from_bottom = (state.scroll_offset as usize).min(max_scroll);
    let top_row = max_scroll.saturating_sub(scroll_from_bottom);
    let bottom_row = top_row + visible_height;

    // Materializing pass: build lines only for messages intersecting the
    // viewport; the paragraph scrolls by the offset into the first visible
    // message instead of into the whole transcript. (Also keeps the scroll
    // argument within u16 even for transcripts longer than 65k rows.)
    let mut lines: Vec<Line> = Vec::new();
    let mut local_offset = 0usize;
    let mut found_first_visible = false;
    for (index, msg) in messages.iter().enumerate() {
        let start = row_starts[index];
        if start >= bottom_row {
            break;
        }
        if start + rows_per_msg[index] <= top_row {
            continue;
        }
        if !found_first_visible {
            found_first_visible = true;
            local_offset = top_row - start;
        }

        if is_markdown_assistant(msg) {
            let (md_lines, tokens) = with_assistant_cached(
                conversation_id,
                index,
                msg,
                wrap_width,
                theme,
                |cached, tokens| (cached.lines.clone(), tokens),
            );
            lines.push(Line::from(vec![assistant_header(msg, theme)]));
            lines.extend(md_lines);
            lines.push(meta_line(msg, tokens, theme));
        } else {
            lines.extend(build_segment_lines(conversation_id, index, msg, theme));
        }
        lines.push(Line::from(""));
    }

    let paragraph = Paragraph::new(lines)
        .style(Style::default().bg(theme.bg))
        .wrap(Wrap { trim: false })
        .scroll((local_offset as u16, 0));
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
        assert!(
            rows > 1,
            "expected long word to force-break, got {rows} rows"
        );
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
        assert_eq!(
            cached_token_estimate(999_002, 0, &msg),
            estimate_tokens(&msg.content)
        );
    }

    /// Manual perf probe: `cargo test --release --bin pooprusteek
    /// render_chat_perf_probe -- --ignored --nocapture`. Renders a fat
    /// transcript frame-by-frame the way fast typing does (one full render
    /// per keystroke) and prints the per-frame cost.
    #[test]
    #[ignore = "manual perf probe, prints timings"]
    fn render_chat_perf_probe() {
        use ratatui::{Terminal, backend::TestBackend};

        let theme = Theme::default_dark();
        let mut state = AppState {
            terminal_rows: 40,
            ..AppState::new(crate::app::conversation::Conversation::fresh_main(None))
        };

        for i in 0..100 {
            state
                .focused_mut()
                .messages
                .push(ChatMessage::user(&format!(
                    "question number {i}: how do I make this considerably faster please?"
                )));
            state.focused_mut().messages.push(ChatMessage::assistant(&format!(
                "Answer {i} with some prose that wraps.\n\n```rust\nfn main() {{\n    println!(\"case {i}\");\n}}\n```\n\nAnd a closing paragraph with **bold** and `inline code` to exercise markdown."
            )));
            state
                .focused_mut()
                .messages
                .push(ChatMessage::system(&format!(
                    "system note {i}\nsecond line of the note"
                )));
        }

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        // Warm the caches (first frame pays markdown+syntect for visible).
        terminal
            .draw(|f| render_chat(f, f.area(), &state, &theme))
            .unwrap();

        let frames = 200u32;
        let start = std::time::Instant::now();
        for _ in 0..frames {
            terminal
                .draw(|f| render_chat(f, f.area(), &state, &theme))
                .unwrap();
        }
        let elapsed = start.elapsed();
        eprintln!(
            "render_chat_perf_probe: {} messages, {:?} total for {frames} frames = {:?}/frame",
            state.focused().messages.len(),
            elapsed,
            elapsed / frames
        );
    }

    #[test]
    fn message_rows_user_counts_content_meta_and_blank() {
        let theme = Theme::default_dark();
        let msg = ChatMessage::user("one\ntwo");
        // 2 content lines + meta line + trailing blank separator.
        assert_eq!(message_rows(777_001, 0, &msg, 80, &theme), 4);
    }

    #[test]
    fn message_rows_assistant_counts_header_body_meta_and_blank() {
        let theme = Theme::default_dark();
        let msg = ChatMessage::assistant("hello");
        // header + 1 markdown line + meta line + trailing blank.
        assert_eq!(message_rows(777_002, 0, &msg, 80, &theme), 4);
    }

    #[test]
    fn message_rows_empty_assistant_is_thinking_placeholder() {
        let theme = Theme::default_dark();
        let msg = ChatMessage::assistant("");
        // one "Thinking..." line + trailing blank.
        assert_eq!(message_rows(777_003, 0, &msg, 80, &theme), 2);
    }

    #[test]
    fn message_rows_stable_across_calls() {
        let theme = Theme::default_dark();
        let msg = ChatMessage::user("cached row count probe");
        let first = message_rows(777_004, 0, &msg, 60, &theme);
        let second = message_rows(777_004, 0, &msg, 60, &theme);
        assert_eq!(first, second);
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
