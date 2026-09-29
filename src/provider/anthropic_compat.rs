//! Anthropic Messages API wire format ⇄ internal provider types — the
//! sibling of [`super::openai_compat`] for the second de-facto standard
//! (the Anthropic API itself and Claude-compatible endpoints/proxies).
//!
//! The format differs from OpenAI's in ways that need real conversion, not
//! just renaming:
//! - the system prompt is a top-level `system` field, never a message;
//! - `messages` roles are only `user`/`assistant` and MUST strictly
//!   alternate starting with `user` — consecutive same-role messages are
//!   merged here; on the prompt path internal `Tool` messages become
//!   labeled `user` text, on the native path (`request.tools` non-empty or
//!   native tool calls already in history) they become structured
//!   `tool_use`/`tool_result` blocks instead;
//! - responses carry a list of typed content blocks;
//! - streaming is typed SSE events (`content_block_delta`,
//!   `message_stop`, …), not uniform chunk deltas.
//!
//! Pure data mapping — no I/O. Transport lives in
//! [`super::anthropic_client`].

use super::compat_client::merge_alternating_turns;
use crate::provider::{ChatMessage, CompletionRequest, CompletionResponse, Role, ToolCall, Usage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Build the Messages-API request body. `ui_only` messages are filtered
/// for the same reason `provider/prompt.rs` filters them: UI chrome must
/// never reach a model.
///
/// Промптовый путь (нет объявленных инструментов и нет нативных вызовов в
/// истории) остаётся байт-в-байт как раньше — иначе сломаются провайдеры,
/// у которых `tools = "prompt"`.
pub fn request_to_anthropic(request: &CompletionRequest) -> Value {
    let has_native_history = request.messages.iter().any(|m| !m.tool_calls.is_empty());
    if request.tools.is_empty() && !has_native_history {
        // Shared with gemini_compat — same merge/opener algorithm, different
        // assistant-role literal.
        let (system_parts, turns) = merge_alternating_turns(request, "assistant");
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
        return body;
    }

    let (system_parts, turns) = build_native_turns(request);
    let messages: Vec<Value> = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "stream": request.stream,
    });
    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }
    if !request.tools.is_empty() {
        // Messages API: {name, description, input_schema} — схема как есть.
        body["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                }))
                .collect::<Vec<_>>()
        );
    }
    body
}

/// Тот же merge/opener алгоритм, что и `merge_alternating_turns`, но блоки
/// контента структурные: `tool_use` у ассистента, `tool_result` у юзера.
/// Нужен свой билдер — общий хелпер (используется и Gemini) плоский текст.
fn build_native_turns(request: &CompletionRequest) -> (Vec<&str>, Vec<(&'static str, Vec<Value>)>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    // Id вызовов, объявленных более ранним сообщением ассистента — только
    // они превращают Tool-сообщение в структурный tool_result.
    let mut known_tool_use_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for message in request.messages.iter().filter(|m| !m.ui_only) {
        let (role, blocks): (&'static str, Vec<Value>) = match message.role {
            Role::System => {
                system_parts.push(&message.content);
                continue;
            }
            Role::User => {
                if message.content.is_empty() {
                    continue;
                }
                (
                    "user",
                    vec![json!({"type": "text", "text": message.content})],
                )
            }
            Role::Assistant => match assistant_blocks(message, &mut known_tool_use_ids) {
                Some(blocks) => ("assistant", blocks),
                None => continue,
            },
            Role::Tool => match tool_blocks(message, &known_tool_use_ids) {
                Some(blocks) => ("user", blocks),
                None => continue,
            },
        };

        match turns.last_mut() {
            Some((last_role, buffer)) if *last_role == role => buffer.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }

    if turns.first().is_none_or(|(role, _)| *role == "assistant") {
        turns.insert(
            0,
            ("user", vec![json!({"type": "text", "text": "(continue)"})]),
        );
    }

    (system_parts, turns)
}

/// Текст (если есть) плюс `tool_use` блок на каждый вызов; запоминает id
/// вызовов, чтобы более поздние Tool-сообщения могли на них сослаться.
fn assistant_blocks(
    message: &ChatMessage,
    known_tool_use_ids: &mut std::collections::HashSet<String>,
) -> Option<Vec<Value>> {
    let mut blocks = Vec::new();
    if !message.content.is_empty() {
        blocks.push(json!({"type": "text", "text": message.content}));
    }
    for call in &message.tool_calls {
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.arguments,
        }));
        known_tool_use_ids.insert(call.id.clone());
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Совпал `tool_call_id` с ранним `tool_use` — структурный `tool_result`.
/// Иначе — старая текстовая форма `[tool result]`, как в промптовом пути.
fn tool_blocks(
    message: &ChatMessage,
    known_tool_use_ids: &std::collections::HashSet<String>,
) -> Option<Vec<Value>> {
    let matched_id = message
        .tool_call_id
        .as_ref()
        .filter(|id| known_tool_use_ids.contains(*id));

    match matched_id {
        Some(id) => {
            let mut block = json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": message.content,
            });
            if message.tool_error {
                block["is_error"] = json!(true);
            }
            Some(vec![block])
        }
        None => {
            if message.content.is_empty() {
                return None;
            }
            Some(vec![json!({
                "type": "text",
                "text": format!("[tool result]\n{}", message.content),
            })])
        }
    }
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

/// Model families that reject sampling parameters (`temperature`/`top_p`/
/// `top_k`) with a 400 — verified against the Anthropic API reference
/// (2026-07): Opus 4.7+, Sonnet 5, and Fable/Mythos 5 removed them ("use
/// prompting instead"). Older Claude models and Claude-compatible proxies
/// still accept them, so the parameter is only dropped for these families.
pub fn model_rejects_sampling(model: &str) -> bool {
    [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
    ]
    .iter()
    .any(|family| model.starts_with(family))
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

/// What one SSE `data:` payload means to the internal stream.
/// `content_block_start/stop`, `message_delta`, `message_stop` now carry
/// tool-call structure; `message_start`/`ping`/thinking blocks stay
/// [`StreamEvent::Ignore`].
#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    /// Append this text to the response.
    Text(String),
    /// A `tool_use` block opened at this index.
    ToolStart {
        index: u64,
        id: String,
        name: String,
    },
    /// More of that block's JSON input arrived.
    ToolDelta {
        index: u64,
        partial_json: String,
    },
    /// That block is complete — its JSON can now be parsed.
    ToolStop {
        index: u64,
    },
    /// The server's stop reason for this turn, already mapped.
    Finish(Option<String>),
    /// The turn is over — emit the terminal stop chunk.
    Done,
    /// The server reported an error mid-stream.
    Error(String),
    Ignore,
}

pub fn parse_stream_event(payload: &str) -> StreamEvent {
    let Ok(event) = serde_json::from_str::<Value>(payload) else {
        return StreamEvent::Ignore;
    };
    let index = || event.get("index").and_then(Value::as_u64).unwrap_or(0);
    match event.get("type").and_then(Value::as_str).unwrap_or("") {
        "content_block_start" => {
            let block = event.get("content_block");
            if block.and_then(|b| b.get("type")).and_then(Value::as_str) != Some("tool_use") {
                return StreamEvent::Ignore;
            }
            let id = block
                .and_then(|b| b.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = block
                .and_then(|b| b.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            StreamEvent::ToolStart {
                index: index(),
                id,
                name,
            }
        }
        "content_block_delta" => {
            let delta = event.get("delta");
            match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                Some("input_json_delta") => {
                    let partial_json = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    StreamEvent::ToolDelta {
                        index: index(),
                        partial_json,
                    }
                }
                _ => {
                    let text = delta
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if text.is_empty() {
                        StreamEvent::Ignore
                    } else {
                        StreamEvent::Text(text.to_string())
                    }
                }
            }
        }
        "content_block_stop" => StreamEvent::ToolStop { index: index() },
        "message_delta" => {
            let stop_reason = event
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
                .map(|reason| map_stop_reason(reason.to_string()));
            StreamEvent::Finish(stop_reason)
        }
        "message_stop" => StreamEvent::Done,
        "error" => StreamEvent::Error(
            event
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider reported an error")
                .to_string(),
        ),
        _ => StreamEvent::Ignore,
    }
}

/// Одно сообщение накапливает вызовы инструментов между событиями стрима.
struct PendingToolCall {
    id: String,
    name: String,
    json_buf: String,
}

/// Состояние одного стрима Anthropic: незакрытые `tool_use` блоки по
/// индексу и причина остановки — заполняются по мере разбора событий.
#[derive(Default)]
pub struct AnthropicStreamState {
    pending: BTreeMap<u64, PendingToolCall>,
    completed: Vec<ToolCall>,
    finish_reason: Option<String>,
}

impl AnthropicStreamState {
    pub fn start_tool(&mut self, index: u64, id: String, name: String) {
        self.pending.insert(
            index,
            PendingToolCall {
                id,
                name,
                json_buf: String::new(),
            },
        );
    }

    pub fn append_tool_json(&mut self, index: u64, partial_json: &str) {
        if let Some(call) = self.pending.get_mut(&index) {
            call.json_buf.push_str(partial_json);
        }
    }

    /// Закрывает блок по индексу и парсит накопленный JSON. Невалидный JSON
    /// не роняет стрим — уходит как строка и в debug_log.
    pub fn finish_tool(&mut self, index: u64) {
        let Some(call) = self.pending.remove(&index) else {
            return;
        };
        let arguments = if call.json_buf.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(&call.json_buf) {
                Ok(value) => value,
                Err(_) => {
                    crate::debug_log::log(
                        "anthropic_compat.tool_json_invalid",
                        format!("id={} raw={}", call.id, call.json_buf),
                    );
                    Value::String(call.json_buf.clone())
                }
            }
        };
        self.completed.push(ToolCall {
            id: call.id,
            name: call.name,
            arguments,
            provider_state: None,
        });
    }

    pub fn set_finish_reason(&mut self, reason: Option<String>) {
        self.finish_reason = reason;
    }

    pub fn finish_reason(&self) -> Option<String> {
        self.finish_reason.clone()
    }

    pub fn take_completed(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.completed)
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
            tools: Vec::new(),
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
            tools: Vec::new(),
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
            tools: Vec::new(),
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
        assert_eq!(
            response_from_anthropic(parsed).finish_reason.as_deref(),
            Some("length")
        );
    }

    #[test]
    fn sampling_rejection_families() {
        // Families verified (2026-07) to 400 on temperature/top_p/top_k.
        assert!(model_rejects_sampling("claude-opus-4-8"));
        assert!(model_rejects_sampling("claude-sonnet-5"));
        assert!(model_rejects_sampling("claude-fable-5"));
        // Older models and compat-proxy models keep sampling support.
        assert!(!model_rejects_sampling("claude-opus-4-6"));
        assert!(!model_rejects_sampling("claude-haiku-4-5"));
        assert!(!model_rejects_sampling("claude-sonnet-4-6"));
        assert!(!model_rejects_sampling("glm-4.7"));
    }

    #[test]
    fn stream_events_decode() {
        assert_eq!(
            parse_stream_event(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tok"}}"#
            ),
            StreamEvent::Text("tok".to_string()),
        );
        assert_eq!(
            parse_stream_event(r#"{"type":"message_stop"}"#),
            StreamEvent::Done
        );
        assert_eq!(
            parse_stream_event(r#"{"type":"ping"}"#),
            StreamEvent::Ignore
        );
        assert_eq!(
            parse_stream_event(r#"{"type":"message_start","message":{"id":"x"}}"#),
            StreamEvent::Ignore,
        );
        assert_eq!(
            parse_stream_event(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
            ),
            StreamEvent::Error("busy".to_string()),
        );
        assert_eq!(parse_stream_event("not json"), StreamEvent::Ignore);
    }

    fn tool_def(name: &str) -> crate::tools::ToolDefinition {
        crate::tools::ToolDefinition {
            name: name.to_string(),
            description: "does a thing".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn tools_declared_with_input_schema() {
        let request = CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: vec![tool_def("get_weather")],
            model: "claude-x".to_string(),
            temperature: 0.5,
            max_tokens: 256,
            stream: false,
        };
        let body = request_to_anthropic(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "does a thing");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn native_round_produces_tool_use_and_merged_tool_result_turn() {
        let mut assistant = ChatMessage::assistant("let me check");
        assistant.tool_calls = vec![
            ToolCall {
                id: "toolu_1".to_string(),
                name: "a".to_string(),
                arguments: json!({"x": 1}),
                provider_state: None,
            },
            ToolCall {
                id: "toolu_2".to_string(),
                name: "b".to_string(),
                arguments: json!({}),
                provider_state: None,
            },
        ];
        let request = CompletionRequest {
            messages: vec![
                ChatMessage::user("check things"),
                assistant,
                ChatMessage::tool("toolu_1", "result a"),
                ChatMessage::tool("toolu_2", "result b"),
                ChatMessage::user("thanks"),
            ],
            tools: vec![tool_def("a")],
            model: "claude-x".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
        };
        let body = request_to_anthropic(&request);
        let messages = body["messages"].as_array().unwrap();
        // user, assistant(text+2 tool_use), user(2 tool_result + text) — alternation holds.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        let assistant_blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks.len(), 3);
        assert_eq!(assistant_blocks[0]["type"], "text");
        assert_eq!(assistant_blocks[1]["type"], "tool_use");
        assert_eq!(assistant_blocks[1]["id"], "toolu_1");
        assert_eq!(assistant_blocks[2]["id"], "toolu_2");

        assert_eq!(messages[2]["role"], "user");
        let result_blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(result_blocks.len(), 3);
        assert_eq!(result_blocks[0]["type"], "tool_result");
        assert_eq!(result_blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(result_blocks[0]["content"], "result a");
        assert_eq!(result_blocks[1]["tool_use_id"], "toolu_2");
        assert_eq!(result_blocks[2]["type"], "text");
        assert_eq!(result_blocks[2]["text"], "thanks");
    }

    #[test]
    fn unmatched_tool_message_stays_text() {
        let request = CompletionRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::tool("orphan_id", "42"),
            ],
            tools: vec![tool_def("a")],
            model: "claude-x".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
        };
        let body = request_to_anthropic(&request);
        let messages = body["messages"].as_array().unwrap();
        // user + orphan tool result merge into one text-only user turn.
        assert_eq!(messages.len(), 1);
        let blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], "text");
        assert!(
            blocks[1]["text"]
                .as_str()
                .unwrap()
                .starts_with("[tool result]")
        );
    }

    #[test]
    fn no_tools_and_no_native_history_is_byte_identical_to_prompt_path() {
        let request = CompletionRequest {
            messages: vec![ChatMessage::user("hi"), ChatMessage::assistant("ok")],
            tools: Vec::new(),
            model: "claude-x".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: false,
        };
        let native = request_to_anthropic(&request);
        let (system_parts, turns) = merge_alternating_turns(&request, "assistant");
        let legacy_messages: Vec<Value> = turns
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect();
        let mut legacy = json!({
            "model": request.model,
            "messages": legacy_messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": request.stream,
        });
        if !system_parts.is_empty() {
            legacy["system"] = json!(system_parts.join("\n\n"));
        }
        assert_eq!(native, legacy);
    }

    #[test]
    fn tool_input_json_arrives_in_three_pieces() {
        let mut state = AnthropicStreamState::default();
        state.start_tool(0, "toolu_1".to_string(), "search".to_string());
        state.append_tool_json(0, r#"{"q":"#);
        state.append_tool_json(0, r#""rust "#);
        state.append_tool_json(0, r#"lang"}"#);
        state.finish_tool(0);
        let calls = state.take_completed();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, json!({"q": "rust lang"}));
    }

    #[test]
    fn text_block_then_tool_use_block_in_one_message() {
        assert_eq!(
            parse_stream_event(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok, "}}"#
            ),
            StreamEvent::Text("ok, ".to_string()),
        );
        assert_eq!(
            parse_stream_event(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"search","input":{}}}"#
            ),
            StreamEvent::ToolStart {
                index: 1,
                id: "toolu_9".to_string(),
                name: "search".to_string()
            },
        );
        assert_eq!(
            parse_stream_event(r#"{"type":"content_block_stop","index":1}"#),
            StreamEvent::ToolStop { index: 1 },
        );
    }

    #[test]
    fn two_tool_use_blocks_stay_independent() {
        let mut state = AnthropicStreamState::default();
        state.start_tool(0, "toolu_a".to_string(), "a".to_string());
        state.start_tool(1, "toolu_b".to_string(), "b".to_string());
        state.append_tool_json(0, r#"{"x":1}"#);
        state.append_tool_json(1, r#"{"y":2}"#);
        state.finish_tool(0);
        state.finish_tool(1);
        let calls = state.take_completed();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "toolu_a");
        assert_eq!(calls[0].arguments, json!({"x": 1}));
        assert_eq!(calls[1].id, "toolu_b");
        assert_eq!(calls[1].arguments, json!({"y": 2}));
    }

    #[test]
    fn message_delta_max_tokens_maps_to_length() {
        assert_eq!(
            parse_stream_event(
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{}}"#
            ),
            StreamEvent::Finish(Some("length".to_string())),
        );
        assert_eq!(
            parse_stream_event(
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#
            ),
            StreamEvent::Finish(Some("tool_use".to_string())),
        );
    }

    #[test]
    fn empty_tool_input_becomes_empty_object() {
        let mut state = AnthropicStreamState::default();
        state.start_tool(0, "toolu_1".to_string(), "noop".to_string());
        state.finish_tool(0);
        let calls = state.take_completed();
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn invalid_tool_json_falls_back_to_string() {
        let mut state = AnthropicStreamState::default();
        state.start_tool(0, "toolu_1".to_string(), "bad".to_string());
        state.append_tool_json(0, "{not json");
        state.finish_tool(0);
        let calls = state.take_completed();
        assert_eq!(calls[0].arguments, Value::String("{not json".to_string()));
    }
}
