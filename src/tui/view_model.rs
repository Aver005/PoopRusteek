//! Derived values for rendering.
//!
//! The status bar and stats panel render numbers that aren't stored on
//! `AppState` directly — token totals, tokens-per-second, the session-id
//! prefix, the MCP label, the spinner frame. These derivations were inlined
//! (and partly duplicated) across `render_mini_status` and the panel; pulling
//! them here gives them one tested home — the first slice of a render-facing
//! view model.

use crate::app::mcp_status::McpStatus;
use crate::provider::{ChatMessage, Role};

/// Total `total_tokens` reported across assistant messages.
pub fn assistant_token_total(messages: &[ChatMessage]) -> u32 {
    messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.total_tokens)
        .sum()
}

/// Tokens per second, guarded against non-positive time and zero tokens.
/// Returns 0.0 when a rate can't be computed.
pub fn tokens_per_sec(tokens: u32, secs: f64) -> f64 {
    if secs > 0.0 && tokens > 0 {
        tokens as f64 / secs
    } else {
        0.0
    }
}

/// The leading 8 characters of a session id (or the whole id if shorter).
pub fn session_prefix(id: &str) -> &str {
    if id.len() >= 8 { &id[..8] } else { id }
}

/// `" mcp:<connected>/<total>"`, or empty when no servers are configured.
pub fn mcp_label(mcp: &McpStatus) -> String {
    if mcp.server_count > 0 {
        format!(" mcp:{}/{}", mcp.connected_count, mcp.server_count)
    } else {
        String::new()
    }
}

/// ASCII spinner frame for the status bar, cycling on `tick`.
pub fn spinner_char(tick: u64) -> &'static str {
    match tick % 4 {
        0 => "|",
        1 => "/",
        2 => "-",
        _ => "\\",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_with_tokens(tokens: Option<u32>) -> ChatMessage {
        let mut m = ChatMessage::assistant("hi");
        m.total_tokens = tokens;
        m
    }

    #[test]
    fn token_total_sums_only_assistant_with_tokens() {
        let msgs = vec![
            ChatMessage::user("q"),                 // ignored (user)
            assistant_with_tokens(Some(10)),
            assistant_with_tokens(None),            // ignored (no count)
            assistant_with_tokens(Some(5)),
        ];
        assert_eq!(assistant_token_total(&msgs), 15);
    }

    #[test]
    fn tps_guards_zero_and_negative() {
        assert_eq!(tokens_per_sec(0, 5.0), 0.0);
        assert_eq!(tokens_per_sec(100, 0.0), 0.0);
        assert_eq!(tokens_per_sec(100, -1.0), 0.0);
        assert!((tokens_per_sec(100, 4.0) - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn session_prefix_truncates_or_passes_through() {
        assert_eq!(session_prefix("0123456789abcdef"), "01234567");
        assert_eq!(session_prefix("short"), "short");
        assert_eq!(session_prefix("12345678"), "12345678");
    }

    #[test]
    fn mcp_label_empty_when_no_servers() {
        let mut m = McpStatus::default();
        assert_eq!(mcp_label(&m), "");
        m.server_count = 3;
        m.connected_count = 2;
        assert_eq!(mcp_label(&m), " mcp:2/3");
    }

    #[test]
    fn spinner_cycles_through_four_frames() {
        let frames: Vec<&str> = (0..5).map(spinner_char).collect();
        assert_eq!(frames, vec!["|", "/", "-", "\\", "|"]);
    }
}
