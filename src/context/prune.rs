use crate::provider::{ChatMessage, Role};
use std::path::Path;

/// How far a body must beat its own marker before clearing it is worth doing.
/// At 1 the trade is break-even; below it the history grows.
const MIN_BODY_MARKER_RATIO: u32 = 2;

/// Marker left where a tool result's body used to be. The call itself stays in
/// the history — the model must still know what it did, even when it no longer
/// knows what came back.
pub fn cleared_marker(path: &str) -> String {
    format!("[tool output cleared to save context — read it back with the read_file tool: {path}]")
}

/// What the marker for one victim costs, spill path included. Derived, not
/// hardcoded: the path is part of the marker, so the floor moves with it.
pub fn marker_tokens(spill_dir: &Path, tool_call_id: Option<&str>, index: usize) -> u32 {
    let path = spill_dir.join(spill_file_name(tool_call_id, index));
    super::budget_tokens(&cleared_marker(&path.to_string_lossy()))
}

/// Already cleared? Prevents re-spilling a marker to disk on the next pass.
pub fn is_cleared(content: &str) -> bool {
    content.starts_with("[tool output cleared to save context")
}

/// Spill file name for one cleared result. The id comes from model output, so
/// everything outside `[A-Za-z0-9_-]` is replaced before it becomes a path.
pub fn spill_file_name(tool_call_id: Option<&str>, index: usize) -> String {
    let safe: String = tool_call_id
        .unwrap_or_default()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = safe.trim_matches('_');
    if stem.is_empty() {
        format!("{index}.txt")
    } else {
        format!("{stem}.txt")
    }
}

/// Start of the exchange in flight: everything after the last assistant
/// message. Same boundary as `provider::prompt::tail_start`.
pub fn in_flight_tail_start(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Which tool results to clear, oldest first.
///
/// Walks backwards accumulating tool-output tokens; everything past
/// `protect_tokens` is fair game. Rung 1 of `.docs/context-compaction.md`.
/// Pure: the caller does the spilling and the rewriting.
///
/// The in-flight tail is excluded before any budget maths: the model has not
/// answered those results yet, so clearing one replies to its own call (§5.3).
///
/// `spill_dir` is not touched — it only sizes the marker each victim would be
/// replaced by, which is the floor a body has to clear to be worth clearing.
pub fn plan_prune(messages: &[ChatMessage], protect_tokens: u32, spill_dir: &Path) -> Vec<usize> {
    let settled = &messages[..in_flight_tail_start(messages)];
    let mut protected = 0u32;
    let mut victims = Vec::new();
    for (index, message) in settled.iter().enumerate().rev() {
        if message.role != Role::Tool || message.ui_only {
            continue;
        }
        if is_cleared(&message.content) {
            continue;
        }
        let cost = super::budget_tokens(&message.content);
        if protected.saturating_add(cost) <= protect_tokens {
            protected = protected.saturating_add(cost);
            continue;
        }
        // A short body — a write confirmation, a refusal — costs less than the
        // marker that would replace it, so clearing it would only add tokens.
        let floor = marker_tokens(spill_dir, message.tool_call_id.as_deref(), index)
            .saturating_mul(MIN_BODY_MARKER_RATIO);
        if cost <= floor {
            continue;
        }
        victims.push(index);
    }
    victims.reverse();
    victims
}

/// Tokens the plan would free — the caller decides whether that is worth a
/// rewrite of the history.
pub fn freed_tokens(messages: &[ChatMessage], victims: &[usize]) -> u32 {
    victims
        .iter()
        .filter_map(|index| messages.get(*index))
        .map(|message| super::budget_tokens(&message.content))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(id: &str, body: &str) -> ChatMessage {
        ChatMessage::tool(id, body)
    }

    fn spill_dir() -> &'static Path {
        Path::new("/spill")
    }

    #[test]
    fn nothing_to_clear_when_everything_fits_the_protected_tail() {
        let messages = vec![
            ChatMessage::user("go"),
            tool_result("1", &"x".repeat(300)),
            ChatMessage::assistant("done"),
        ];
        assert!(plan_prune(&messages, 1_000, spill_dir()).is_empty());
    }

    #[test]
    fn oldest_results_are_cleared_first_and_the_newest_survive() {
        // 300 chars ≈ 100 budget tokens each. The trailing assistant reply is
        // what makes all three settled rather than in flight.
        let messages = vec![
            tool_result("1", &"a".repeat(300)),
            tool_result("2", &"b".repeat(300)),
            tool_result("3", &"c".repeat(300)),
            ChatMessage::assistant("so far so good"),
        ];
        // Room for the last two only.
        let victims = plan_prune(&messages, 250, spill_dir());
        assert_eq!(victims, vec![0]);

        // Room for none: everything but the walk's first fit goes.
        let victims = plan_prune(&messages, 50, spill_dir());
        assert_eq!(victims, vec![0, 1, 2]);
    }

    #[test]
    fn non_tool_messages_are_never_touched() {
        let messages = vec![
            ChatMessage::user(&"u".repeat(3_000)),
            ChatMessage::assistant(&"a".repeat(3_000)),
            ChatMessage::system(&"s".repeat(3_000)),
        ];
        assert!(plan_prune(&messages, 0, spill_dir()).is_empty());
    }

    #[test]
    fn an_already_cleared_result_is_not_planned_again() {
        let messages = vec![
            tool_result("1", &cleared_marker("C:/data/tool-output/1.txt")),
            tool_result("2", &"b".repeat(3_000)),
            ChatMessage::assistant("read them"),
        ];
        let victims = plan_prune(&messages, 0, spill_dir());
        assert_eq!(victims, vec![1], "only the uncleared one is a victim");
    }

    /// Property: results the model has not answered yet are off limits even
    /// with a zero budget. Remove the tail guard and this goes red.
    #[test]
    fn results_after_the_last_assistant_message_are_never_planned() {
        let messages = vec![
            tool_result("1", &"a".repeat(3_000)),
            ChatMessage::assistant("<tool_use>…</tool_use>"),
            tool_result("2", &"b".repeat(3_000)),
            tool_result("3", &"c".repeat(3_000)),
        ];
        assert_eq!(plan_prune(&messages, 0, spill_dir()), vec![0]);
    }

    #[test]
    fn a_history_without_any_assistant_reply_is_entirely_in_flight() {
        let messages = vec![
            ChatMessage::user("go"),
            tool_result("1", &"a".repeat(3_000)),
        ];
        assert!(plan_prune(&messages, 0, spill_dir()).is_empty());
    }

    /// A body smaller than the marker that would replace it is not a saving.
    /// Short results are ordinary: write confirmations, refusals, skips.
    #[test]
    fn a_body_no_bigger_than_its_own_marker_is_left_alone() {
        let messages = vec![
            tool_result("1", "Wrote 3 lines to notes.txt"),
            tool_result("2", &"b".repeat(3_000)),
            ChatMessage::assistant("go on"),
        ];
        assert_eq!(
            plan_prune(&messages, 0, spill_dir()),
            vec![1],
            "clearing the short result would make the history longer"
        );
        assert!(
            marker_tokens(spill_dir(), Some("1"), 0)
                < marker_tokens(Path::new("/a/much/longer/spill/directory"), Some("1"), 0),
            "the floor is derived from the marker, path included"
        );
    }

    #[test]
    fn freed_tokens_counts_only_the_planned_victims() {
        let messages = vec![
            tool_result("1", &"a".repeat(300)),
            tool_result("2", &"b".repeat(600)),
        ];
        assert_eq!(freed_tokens(&messages, &[0]), 100);
        assert_eq!(freed_tokens(&messages, &[0, 1]), 300);
        assert_eq!(freed_tokens(&messages, &[]), 0);
    }

    #[test]
    fn the_marker_is_recognised_as_cleared() {
        let marker = cleared_marker("/tmp/out.txt");
        assert!(is_cleared(&marker));
        assert!(marker.contains("/tmp/out.txt"));
        assert!(
            marker.contains("read_file"),
            "names the tool that fetches it"
        );
        assert!(!is_cleared("ordinary tool output"));
    }

    #[test]
    fn a_spill_name_never_escapes_the_spill_directory() {
        assert_eq!(spill_file_name(Some("call_ab-1"), 7), "call_ab-1.txt");
        assert_eq!(spill_file_name(Some(".."), 7), "7.txt");
        assert_eq!(
            spill_file_name(Some("../../etc/passwd"), 7),
            "etc_passwd.txt"
        );
        assert_eq!(spill_file_name(Some("a:b?c*d"), 7), "a_b_c_d.txt");
        assert_eq!(spill_file_name(None, 7), "7.txt");
        assert_eq!(spill_file_name(Some(""), 7), "7.txt");
        assert!(spill_file_name(Some(&"x".repeat(500)), 7).len() <= 68);
    }
}
