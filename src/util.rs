/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8 char.
/// Returns a slice ending on a char boundary. Use instead of `&s[..n]` for any
/// content that may contain multibyte UTF-8 (PTY/ANSI output, JSON bodies, etc).
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    &s[..s.floor_char_boundary(max_bytes)]
}

/// Truncate to `max_bytes` with an ellipsis suffix, char-boundary safe.
pub fn truncate_with_ellipsis(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = max_bytes.saturating_sub(3);
    let head = truncate_at_char_boundary(s, cut);
    format!("{head}...")
}
