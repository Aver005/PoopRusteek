//! Перенос истории при смене цели внутри беседы.
//!
//! Контракт: `provider_state` (подписи Gemini), и `tool_calls` с чужими id нельзя отдавать
//! другому аккаунту или модели. Перед первым вызовом новой цели фасад прогоняет историю
//! через `flatten_for_switch`. Сообщения остаются, меняется только их форма.

use crate::provider::{ChatMessage, Role};

/// Есть ли в истории то, что чужая цель не поймёт.
pub fn carries_foreign_state(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == Role::Assistant
            && m.tool_calls
                .iter()
                .any(|call| !call.id.is_empty() || call.provider_state.is_some())
    })
}

/// История без привязки к прежней цели: вызовы инструментов разворачиваются в блоки
/// `<tool_use>` (как их пишет промптовый протокол), подписи и id вызовов исчезают.
pub fn flatten_for_switch(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|message| {
            if message.role != Role::Assistant || message.tool_calls.is_empty() {
                return message.clone();
            }
            let mut flat = message.clone();
            for call in &message.tool_calls {
                if !flat.content.is_empty() {
                    flat.content.push('\n');
                }
                flat.content.push_str(&format!(
                    "<tool_use><name>{}</name><arguments>{}</arguments></tool_use>",
                    call.name, call.arguments
                ));
            }
            flat.tool_calls.clear();
            flat
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": "a.rs"}),
            provider_state: Some(json!({"thoughtSignature": "sig"})),
        }
    }

    #[test]
    fn calls_become_prompt_blocks_and_signatures_disappear() {
        let mut assistant = ChatMessage::assistant("looking");
        assistant.tool_calls = vec![call("call_1")];
        let history = vec![
            ChatMessage::user("q"),
            assistant,
            ChatMessage::tool("call_1", "ok"),
        ];
        assert!(carries_foreign_state(&history));
        let flat = flatten_for_switch(&history);
        assert!(flat[1].tool_calls.is_empty());
        assert!(
            flat[1]
                .content
                .starts_with("looking\n<tool_use><name>read_file</name>")
        );
        assert!(flat[1].content.contains(r#"{"path":"a.rs"}"#));
        assert!(!carries_foreign_state(&flat), "nothing foreign is left");
        assert_eq!(flat[2].content, "ok", "results stay as they were");
    }

    #[test]
    fn plain_history_is_untouched_and_flattening_is_idempotent() {
        let history = vec![ChatMessage::user("q"), ChatMessage::assistant("a")];
        assert!(!carries_foreign_state(&history));
        let once = flatten_for_switch(&history);
        assert_eq!(once[1].content, "a");
        let mut with_call = ChatMessage::assistant("");
        with_call.tool_calls = vec![call("c")];
        let first = flatten_for_switch(&[with_call]);
        let second = flatten_for_switch(&first);
        assert_eq!(first[0].content, second[0].content);
    }
}
