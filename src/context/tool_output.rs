/// Share of the budget kept from the head. The tail carries the exit status,
/// the error, the summary line — so it gets the rest. Roo's `truncateOutput`
/// splits 20/80 for the same reason.
const HEAD_SHARE: usize = 20;

/// Cap a tool result before it enters the history, keeping both ends.
///
/// Rung 0 of the compaction ladder (`.docs/context-compaction.md`): trimming at
/// capture is what keeps a 200 KB `cat` from ever reaching a summariser. The
/// caller keeps the untrimmed text for the UI — only what the model sees is cut.
pub fn cap_tool_output(text: &str, limit_chars: usize) -> String {
    if limit_chars == 0 {
        return text.to_string();
    }
    let total = text.chars().count();
    if total <= limit_chars {
        return text.to_string();
    }

    let head_chars = limit_chars * HEAD_SHARE / 100;
    let tail_chars = limit_chars - head_chars;
    let dropped = total - limit_chars;

    let head_end = char_offset(text, head_chars);
    let tail_start = char_offset(text, total - tail_chars);
    format!(
        "{}\n[... {dropped} chars cut from the middle; the tool ran, only this text was trimmed ...]\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Byte offset of the `n`-th char. Never slices mid-character (invariant 4).
fn char_offset(text: &str, n: usize) -> usize {
    text.char_indices()
        .nth(n)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(cap_tool_output("ok", 100), "ok");
        // Exactly at the limit is not truncation.
        assert_eq!(cap_tool_output("abcde", 5), "abcde");
    }

    #[test]
    fn zero_limit_disables_the_cap() {
        let long = "x".repeat(10_000);
        assert_eq!(cap_tool_output(&long, 0), long);
    }

    #[test]
    fn both_ends_survive_and_the_marker_reports_the_loss() {
        let text = format!("HEAD{}TAIL", "m".repeat(500));
        let capped = cap_tool_output(&text, 100);
        assert!(capped.starts_with("HEAD"), "head kept: {capped}");
        assert!(capped.ends_with("TAIL"), "tail kept: {capped}");
        assert!(capped.contains("408 chars cut"), "marker: {capped}");
    }

    #[test]
    fn multibyte_output_is_cut_on_char_boundaries() {
        // 600 Cyrillic chars: byte slicing would split a 2-byte char and panic.
        let text = "я".repeat(600);
        let capped = cap_tool_output(&text, 100);
        assert!(capped.contains("500 chars cut"));
        assert!(capped.starts_with(&"я".repeat(20)));
        assert!(capped.ends_with(&"я".repeat(80)));
    }

    #[test]
    fn emoji_survive_the_cut() {
        let text = format!("🙂🙂{}🙃🙃", "-".repeat(300));
        let capped = cap_tool_output(&text, 20);
        assert!(capped.starts_with("🙂🙂"));
        assert!(capped.ends_with("🙃🙃"));
    }
}
