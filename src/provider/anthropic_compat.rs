//! Anthropic Messages API wire format ⇄ internal provider types — the
//! sibling of [`super::openai_compat`] for the second de-facto standard
//! (the Anthropic API itself and Claude-compatible endpoints/proxies).
//!
//! The format differs from OpenAI's in ways that need real conversion, not
//! just renaming:
//! - the system prompt is a top-level `system` field, never a message;
//! - `messages` roles are only `user`/`assistant` and MUST strictly
//!   alternate starting with `user` — consecutive same-role messages are
//!   merged here, and internal `Tool` messages become labeled `user` text
//!   (pooprusteek's tool protocol is prompt-encoded; Anthropic's
//!   structured `tool_use`/`tool_result` blocks are out of scope, same v1
//!   stance as `openai_compat`);
//! - responses carry a list of typed content blocks;
//! - streaming is typed SSE events (`content_block_delta`,
//!   `message_stop`, …), not uniform chunk deltas.
//!
//! Pure data mapping — no I/O. Transport lives in
//! [`super::anthropic_client`].

use crate::provider::{CompletionRequest, CompletionResponse, Role, Usage};
use serde::Deserialize;
use serde_json::{json, Value};

/// Build the Messages-API request body. `ui_only` messages are filtered
/// for the same reason `provider/prompt.rs` filters them: UI chrome must
/// never reach a model.
pub fn request_to_anthropic(request: &CompletionRequest) -> Value {
    let mut system_parts: Vec<&str> = Vec::new();
    // (role, accumulated text) with consecutive same-role runs merged.
    let mut turns: Vec<(&'static str, String)> = Vec::new();

    for message in request.messages.iter().filter(|message| !message.ui_only) {
        let (role, text) = match message.role {
            Role::System => {
                system_parts.push(&message.content);
                continue;
            }
            Role::User => ("user", message.content.clone()),
            Role::Assistant => ("assistant", message.content.clone()),
            // Prompt-encoded tool protocol: results go back as user text,
            // labeled so the model can tell them from the human.
            Role::Tool => ("user", format!("[tool result]\n{}", message.content)),
        };
        if text.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last_role, buffer)) if *last_role == role => {
                buffer.push_str("\n\n");
                buffer.push_str(&text);
            }
            _ => turns.push((role, text)),
        }
    }

    // The API rejects an assistant-first (or empty) conversation.
    if matches!(turns.first(), Some(("assistant", _)) | None) {
        turns.insert(0, ("user", "(continue)".to_string()));
    }

    let messages: Vec<Value> = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        // Required by the Messages API (unlike OpenAI's optional field).
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "stream": request.stream,
    });
    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }
    body
}

// ── Response (non-streaming) ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MessagesResponse {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

/// Map Anthropic stop reasons onto the OpenAI-style vocabulary the rest of
/// the app speaks ("stop"/"length"); unknown reasons pass through.
fn map_stop_reason(reason: String) -> String {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        _ => reason,
    }
}

pub fn response_from_anthropic(response: MessagesResponse) -> CompletionResponse {
    let content: String = response
        .content
        .into_iter()
        .filter(|block| block.kind == "text")
        .filter_map(|block| block.text)
        .collect::<Vec<_>>()
        .join("");
    CompletionResponse {
        content,
        finish_reason: response.stop_reason.map(map_stop_reason),
        usage: response.usage.map(|usage| Usage {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            total_tokens: usage.input_tokens + usage.output_tokens,
        }),
    }
}

// ── Streaming events ────────────────────────────────────────────────────

/// What one SSE `data:` payload means to the internal stream. The
/// transport only needs three outcomes; everything else (`message_start`,
/// `content_block_start/stop`, `ping`, …) is [`StreamEvent::Ignore`].
#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    /// Append this text to the response.
    Text(String),
    /// The turn is over — emit the terminal stop chunk.
    Done,
    /// The server reported an error mid-stream.
    Error(String),
    Ignore,
}

pub fn parse_stream_event(payload: &str) -> StreamEvent {
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        delta: Option<Value>,
        #[serde(default)]
        error: Option<Value>,
    }

    let Ok(event) = serde_json::from_str::<Event>(payload) else {
        return StreamEvent::Ignore;
    };
    match event.kind.as_str() {
        "content_block_delta" => {
            let text = event
                .delta
                .as_ref()
                .and_then(|delta| delta.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if text.is_empty() {
                StreamEvent::Ignore
            } else {
                StreamEvent::Text(text.to_string())
            }
        }
        "message_stop" => StreamEvent::Done,
        "error" => StreamEvent::Error(
            event
                .error
                .as_ref()
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider reported an error")
                .to_string(),
        ),
        _ => StreamEvent::Ignore,
    }
}

// ── Models listing ──────────────────────────────────────────────────────

/// `GET {base}/models` page — same `data[].id` core as OpenAI's list.
#[derive(Debug, Deserialize)]
pub struct ModelsPage {
    #[serde(default)]
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    #[test]
    fn system_goes_top_level_and_roles_merge() {
        let request = CompletionRequest {
            messages: vec![
                ChatMessage::system("be terse"),
                ChatMessage::system("answer in Russian"),
                ChatMessage::user("hi"),
                ChatMessage::tool("call_1", "42"),
                ChatMessage::assistant("ok"),
            ],
            model: "claude-x".to_string(),
            temperature: 0.5,
            max_tokens: 256,
            stream: false,
        };
        let body = request_to_anthropic(&request);

        assert_eq!(body["system"], "be terse\n\nanswer in Russian");
        assert_eq!(body["max_tokens"], 256);
        let messages = body["messages"].as_array().unwrap();
        // user + tool merged into one user turn, then assistant.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        let first = messages[0]["content"].as_str().unwrap();
        assert!(first.starts_with("hi\n\n[tool result]"));
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn assistant_first_conversation_gets_a_user_opener() {
        let request = CompletionRequest {
            messages: vec![ChatMessage::assistant("previous answer")],
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: true,
        };
        let body = request_to_anthropic(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn ui_only_messages_are_filtered() {
        let mut chrome = ChatMessage::user("ui chrome");
        chrome.ui_only = true;
        let request = CompletionRequest {
            messages: vec![chrome, ChatMessage::user("real")],
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
        };
        let body = request_to_anthropic(&request);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"], "real");
    }

    #[test]
    fn response_concatenates_text_blocks_and_maps_stop_reason() {
        let raw = r#"{
            "content": [
                {"type": "text", "text": "Hel"},
                {"type": "thinking", "thinking": "..."},
                {"type": "text", "text": "lo"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let parsed: MessagesResponse = serde_json::from_str(raw).unwrap();
        let internal = response_from_anthropic(parsed);
        assert_eq!(internal.content, "Hello");
        assert_eq!(internal.finish_reason.as_deref(), Some("stop"));
        let usage = internal.usage.unwrap();
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn max_tokens_stop_reason_maps_to_length() {
        let parsed: MessagesResponse =
            serde_json::from_str(r#"{"content": [], "stop_reason": "max_tokens"}"#).unwrap();
        assert_eq!(response_from_anthropic(parsed).finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn stream_events_decode() {
        assert_eq!(
            parse_stream_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tok"}}"#),
            StreamEvent::Text("tok".to_string()),
        );
        assert_eq!(parse_stream_event(r#"{"type":"message_stop"}"#), StreamEvent::Done);
        assert_eq!(parse_stream_event(r#"{"type":"ping"}"#), StreamEvent::Ignore);
        assert_eq!(
            parse_stream_event(r#"{"type":"message_start","message":{"id":"x"}}"#),
            StreamEvent::Ignore,
        );
        assert_eq!(
            parse_stream_event(r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#),
            StreamEvent::Error("busy".to_string()),
        );
        assert_eq!(parse_stream_event("not json"), StreamEvent::Ignore);
    }
}
