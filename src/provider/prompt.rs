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

/// Build the flat prompt string sent to DeepSeek.
///
/// On the first turn of a session (`system_sent_for_session == false`) the
/// system prompt and prior turns are embedded as local memory. On later turns
/// only the newest user input or tool-result batch is sent, since DeepSeek
/// retains the rest server-side.
pub(crate) fn build_prompt(
    messages: &[ChatMessage],
    system_prompt: &str,
    system_sent_for_session: bool,
) -> String {
    let Some(last_message) = messages.last() else {
        return system_prompt.trim().to_string();
    };

    if !system_sent_for_session {
        let mut parts = Vec::new();

        if !system_prompt.trim().is_empty() {
            parts.push(system_prompt.trim().to_string());
        }

        if messages.len() > 1 {
            let history = messages[..messages.len() - 1]
                .iter()
                .map(format_history_message)
                .collect::<Vec<_>>()
                .join("\n\n");
            parts.push(String::new());
            parts.push("### LOCAL MEMORY".to_string());
            parts.push(history);
        }

        if last_message.role == Role::Tool {
            parts.push(String::new());
            parts.push(format!(
                "### TOOL RESULT: {}",
                last_message.name.as_deref().unwrap_or("unknown")
            ));
            parts.push(last_message.content.clone());
        } else if !last_message.content.is_empty() {
            parts.push(String::new());
            parts.push("### USER INPUT".to_string());
            parts.push(last_message.content.clone());
        }

        return parts.join("\n");
    }

    if last_message.role == Role::Tool {
        let mut tool_batch = Vec::new();
        for message in messages.iter().rev() {
            if message.role != Role::Tool {
                break;
            }
            tool_batch.push(message);
        }
        tool_batch.reverse();

        return tool_batch
            .into_iter()
            .map(|message| {
                format!(
                    "### TOOL RESULT: {}\n{}",
                    message.name.as_deref().unwrap_or("unknown"),
                    message.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    if last_message.content.is_empty() {
        return last_message.content.clone();
    }

    format!("### USER INPUT\n{}", last_message.content)
}

/// Map a model name + session position to DeepSeek's `model_type` field.
/// Reasoner/expert models think; the first message of a session uses
/// `default`; continuations omit the field.
pub(crate) fn resolve_model_type(model: &str, parent_message_id: Option<i64>) -> Option<&'static str> {
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
        assert_eq!(prompt, "### USER INPUT\nnew");
    }

    #[test]
    fn later_turn_batches_trailing_tool_results() {
        let messages = vec![
            ChatMessage::user("q"),
            ChatMessage::tool_with_display("id1", "bash", "out-a", "out-a", false),
            ChatMessage::tool_with_display("id2", "grep", "out-b", "out-b", false),
        ];
        let prompt = build_prompt(&messages, "SYS", true);
        assert_eq!(
            prompt,
            "### TOOL RESULT: bash\nout-a\n\n### TOOL RESULT: grep\nout-b"
        );
    }

    #[test]
    fn empty_messages_returns_trimmed_system() {
        assert_eq!(build_prompt(&[], "  sys  ", false), "sys");
    }

    #[test]
    fn long_code_blocks_collapse_in_history() {
        let big = format!("```\n{}\n```", "x".repeat(400));
        let messages = vec![
            ChatMessage::assistant(&big),
            ChatMessage::user("now"),
        ];
        let prompt = build_prompt(&messages, "", false);
        assert!(prompt.contains("[...]"));
        assert!(!prompt.contains(&"x".repeat(400)));
    }

    #[test]
    fn model_type_resolution() {
        assert_eq!(resolve_model_type("deepseek-reasoner", None), Some("expert"));
        assert_eq!(resolve_model_type("some-expert", Some(5)), Some("expert"));
        assert_eq!(resolve_model_type("deepseek-chat", None), Some("default"));
        assert_eq!(resolve_model_type("deepseek-chat", Some(5)), None);
    }
}
