//! DeepSeek prompt assembly.
//!
//! DeepSeek's web API takes a single flat `prompt` string rather than a
//! role-tagged message array, and treats the first turn of a session
//! differently from later ones (the system prompt and local history are only
//! sent once). This module owns that translation from [`ChatMessage`]s to the
//! wire prompt. It is pure — no `self`, no I/O — so the bug-prone assembly
//! rules live in one place and are covered by tests.

use super::{ChatMessage, Role};
use regex::Regex;
use std::sync::LazyLock;

/// Code fences of 300+ chars are collapsed to `[...]` when replaying assistant
/// history, so old code dumps don't blow up the prompt budget.
static LONG_CODE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.{300,}?```").expect("hardcoded regex is valid"));

/// Pull the first `System` message out as the system prompt; return the rest in
/// order. Later system messages (if any) stay in the non-system list.
pub(crate) fn split_system_prompt(messages: &[ChatMessage]) -> (String, Vec<ChatMessage>) {
    let mut system_prompt = String::new();
    let mut non_system = Vec::new();
    let mut captured_system = false;

    for message in messages {
        if !captured_system && message.role == Role::System {
            system_prompt = message.content.clone();
            captured_system = true;
        } else {
            non_system.push(message.clone());
        }
    }

    (system_prompt, non_system)
}

fn strip_long_code_blocks(text: &str) -> String {
    LONG_CODE_BLOCK_RE.replace_all(text, "[...]").into_owned()
}

fn format_history_message(message: &ChatMessage) -> String {
    if message.role == Role::Assistant {
        let stripped = strip_long_code_blocks(message.content.trim());
        return format!("[ASSISTANT]\n{stripped}");
    }

    let role = match message.role {
        Role::System => "SYSTEM",
        Role::User => "USER",
        Role::Assistant => "ASSISTANT",
        Role::Tool => "TOOL",
    };
    format!("[{role}]\n{}", message.content)
}

/// Trailing one-line format anchor appended to every non-empty send. The
/// system prompt is delivered once per session and then only lives far back
/// in DeepSeek's server-side context; a weak model holds the tool-call
/// format by recency far better than by primacy, and this costs ~60 tokens
/// per turn.
const FORMAT_REMINDER: &str = "[Напоминание формата: инструменты вызывай только блоком <tool_use><name>…</name><arguments>{JSON}</arguments></tool_use>, после </tool_use> — стоп. Имена инструментов не изобретай, результаты не выдумывай — жди TOOL RESULT.]";

/// Index where the "new input" tail begins: everything after the last
/// assistant message. That tail is what a continuing session actually needs
/// to send — typically one user message, a batch of tool results, or an
/// advisory system note (semantic hint) followed by the user message.
fn tail_start(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Render one tail message as a labeled section. `None` for empty user/system
/// content (nothing to say) and for assistant messages (excluded from the
/// tail by construction). Tool results keep an explicit section even when
/// empty so the model sees the call completed.
fn format_tail_message(message: &ChatMessage) -> Option<String> {
    match message.role {
        Role::Tool => Some(format!(
            "### TOOL RESULT: {}\n{}",
            message.name.as_deref().unwrap_or("unknown"),
            message.content
        )),
        Role::User if !message.content.is_empty() => {
            Some(format!("### USER INPUT\n{}", message.content))
        }
        Role::System if !message.content.is_empty() => {
            Some(format!("### NOTE\n{}", message.content))
        }
        _ => None,
    }
}

fn render_tail(tail: &[ChatMessage]) -> String {
    tail.iter()
        .filter_map(format_tail_message)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// True when the local transcript holds exactly one real conversational
/// message (the newest user input) — i.e. this is the first send of a brand
/// new conversation on a possibly-reused provider session. Advisory system
/// notes (semantic hints) are not conversational and don't count.
pub(crate) fn is_first_conversational_send(non_system_messages: &[ChatMessage]) -> bool {
    non_system_messages
        .iter()
        .filter(|m| m.role != Role::System)
        .count()
        == 1
}

/// Build the flat prompt string sent to DeepSeek.
///
/// On the first send of a session (`system_sent_for_session == false`) the
/// system prompt and prior turns are embedded as local memory. On later sends
/// only the tail after the last assistant message goes out (the newest user
/// input, tool-result batch, and any system note), since DeepSeek retains the
/// rest server-side. A user message in the tail always renders as its own
/// `### USER INPUT` section — system notes must never displace it.
pub(crate) fn build_prompt(
    messages: &[ChatMessage],
    system_prompt: &str,
    system_sent_for_session: bool,
) -> String {
    if messages.is_empty() {
        return system_prompt.trim().to_string();
    }

    let tail_from = tail_start(messages);
    let tail = render_tail(&messages[tail_from..]);

    if system_sent_for_session {
        if tail.is_empty() {
            return tail;
        }
        return format!("{tail}\n\n{FORMAT_REMINDER}");
    }

    let mut parts = Vec::new();

    if !system_prompt.trim().is_empty() {
        parts.push(system_prompt.trim().to_string());
    }

    if tail_from > 0 {
        let history = messages[..tail_from]
            .iter()
            .map(format_history_message)
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(String::new());
        parts.push("### LOCAL MEMORY".to_string());
        parts.push(history);
    }

    if !tail.is_empty() {
        parts.push(String::new());
        parts.push(tail);
        parts.push(String::new());
        parts.push(FORMAT_REMINDER.to_string());
    }

    parts.join("\n")
}

/// Map a model name + session position to DeepSeek's `model_type` field.
/// Reasoner/expert models think; the first message of a session uses
/// `default`; continuations omit the field.
pub(crate) fn resolve_model_type(
    model: &str,
    parent_message_id: Option<i64>,
) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.contains("reasoner") || lower.contains("expert") {
        Some("expert")
    } else if parent_message_id.is_none() {
        Some("default")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_captures_first_system_only() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::system("late"),
        ];
        let (system, rest) = split_system_prompt(&messages);
        assert_eq!(system, "sys");
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].role, Role::User);
        assert_eq!(rest[1].role, Role::System); // a later system stays in the list
    }

    #[test]
    fn first_turn_embeds_system_and_user_input() {
        let messages = vec![ChatMessage::user("hello")];
        let prompt = build_prompt(&messages, "SYSTEM PROMPT", false);
        assert!(prompt.starts_with("SYSTEM PROMPT"));
        assert!(prompt.contains("### USER INPUT\nhello"));
        assert!(!prompt.contains("### LOCAL MEMORY")); // single message → no history
    }

    #[test]
    fn first_turn_with_history_includes_local_memory() {
        let messages = vec![
            ChatMessage::user("earlier"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("now"),
        ];
        let prompt = build_prompt(&messages, "SYS", false);
        assert!(prompt.contains("### LOCAL MEMORY"));
        assert!(prompt.contains("[ASSISTANT]\nreply"));
        assert!(prompt.contains("### USER INPUT\nnow"));
    }

    #[test]
    fn later_turn_sends_only_latest_user_input() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user("new"),
        ];
        let prompt = build_prompt(&messages, "SYS", true);
        assert_eq!(prompt, format!("### USER INPUT\nnew\n\n{FORMAT_REMINDER}"));
        assert!(!prompt.contains("old")); // server already has earlier turns
    }

    #[test]
    fn later_turn_batches_trailing_tool_results() {
        let messages = vec![
            ChatMessage::user("q"),
            ChatMessage::assistant("<tool_use>…</tool_use>"),
            ChatMessage::tool_with_display("id1", "bash", "out-a", "out-a", false),
            ChatMessage::tool_with_display("id2", "grep", "out-b", "out-b", false),
        ];
        let prompt = build_prompt(&messages, "SYS", true);
        assert_eq!(
            prompt,
            format!(
                "### TOOL RESULT: bash\nout-a\n\n### TOOL RESULT: grep\nout-b\n\n{FORMAT_REMINDER}"
            )
        );
    }

    /// Regression: the semantic hint is inserted *before* the newest user
    /// message; the user text must go out as its own `### USER INPUT`
    /// section, after the hint's `### NOTE`.
    #[test]
    fn hint_before_user_keeps_user_input_last() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::system("[Hint] maybe use skill X"),
            ChatMessage::user("real question"),
        ];
        let prompt = build_prompt(&messages, "SYS", true);
        assert_eq!(
            prompt,
            format!(
                "### NOTE\n[Hint] maybe use skill X\n\n### USER INPUT\nreal question\n\n{FORMAT_REMINDER}"
            )
        );
    }

    /// Regression for the original bug: a trailing system note after the
    /// user message used to be sent as the sole `### USER INPUT`, silently
    /// dropping the user's actual message.
    #[test]
    fn trailing_system_note_never_displaces_user_input() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user("real question"),
            ChatMessage::system("[Hint] maybe use skill X"),
        ];
        let prompt = build_prompt(&messages, "SYS", true);
        assert!(prompt.contains("### USER INPUT\nreal question"));
        assert!(prompt.contains("### NOTE\n[Hint] maybe use skill X"));
    }

    #[test]
    fn first_send_renders_hint_as_note_not_memory() {
        let messages = vec![
            ChatMessage::system("[Hint] maybe use skill X"),
            ChatMessage::user("hello"),
        ];
        let prompt = build_prompt(&messages, "SYSTEM PROMPT", false);
        assert!(prompt.starts_with("SYSTEM PROMPT"));
        assert!(prompt.contains("### NOTE\n[Hint] maybe use skill X"));
        assert!(prompt.contains("### USER INPUT\nhello"));
        assert!(!prompt.contains("### LOCAL MEMORY"));
    }

    #[test]
    fn empty_user_content_on_later_turn_sends_nothing() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user(""),
        ];
        assert_eq!(build_prompt(&messages, "SYS", true), "");
    }

    #[test]
    fn first_conversational_send_ignores_system_notes() {
        assert!(is_first_conversational_send(&[ChatMessage::user("hi")]));
        assert!(is_first_conversational_send(&[
            ChatMessage::system("[Hint] x"),
            ChatMessage::user("hi"),
        ]));
        assert!(!is_first_conversational_send(&[
            ChatMessage::user("hi"),
            ChatMessage::assistant("a"),
            ChatMessage::user("more"),
        ]));
        assert!(!is_first_conversational_send(&[]));
    }

    #[test]
    fn empty_messages_returns_trimmed_system() {
        assert_eq!(build_prompt(&[], "  sys  ", false), "sys");
    }

    #[test]
    fn long_code_blocks_collapse_in_history() {
        let big = format!("```\n{}\n```", "x".repeat(400));
        let messages = vec![ChatMessage::assistant(&big), ChatMessage::user("now")];
        let prompt = build_prompt(&messages, "", false);
        assert!(prompt.contains("[...]"));
        assert!(!prompt.contains(&"x".repeat(400)));
    }

    #[test]
    fn model_type_resolution() {
        assert_eq!(
            resolve_model_type("deepseek-reasoner", None),
            Some("expert")
        );
        assert_eq!(resolve_model_type("some-expert", Some(5)), Some("expert"));
        assert_eq!(resolve_model_type("deepseek-chat", None), Some("default"));
        assert_eq!(resolve_model_type("deepseek-chat", Some(5)), None);
    }
}
