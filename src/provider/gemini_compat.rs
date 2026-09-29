//! Google Generative Language API (Gemini) wire format ⇄ internal provider
//! types — the third compat pair after [`super::openai_compat`] and
//! [`super::anthropic_compat`].
//!
//! Format differences that need real conversion:
//! - history is `contents[].parts[].text` with roles `user`/`model` (not
//!   `assistant`), and multi-turn requests must alternate — consecutive
//!   same-role turns are merged here;
//! - the system prompt is a top-level `systemInstruction`, never a turn;
//! - sampling knobs live under `generationConfig`
//!   (`temperature`/`maxOutputTokens`);
//! - the model id is part of the URL (`models/{model}:generateContent`),
//!   not the body — the transport owns that part;
//! - streaming (`:streamGenerateContent?alt=sse`) sends the SAME response
//!   shape per SSE `data:` line (no typed events, no `[DONE]`) — the final
//!   chunk carries `finishReason`.
//! - native tools declare as `tools[0].functionDeclarations[]`; a model turn
//!   with calls carries `functionCall` parts, and their results go back as
//!   `functionResponse` parts in a `user` turn (Gemini has no separate "tool"
//!   role). This file builds that history itself — `compat_client`'s
//!   `merge_alternating_turns` only knows plain text and is shared with
//!   Anthropic, so it stays untouched.
//!
//! Pure data mapping — no I/O. Transport lives in [`super::gemini_client`].

use crate::provider::{CompletionRequest, CompletionResponse, Role, ToolCall, Usage};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Build the `generateContent` request body. `ui_only` messages are
/// filtered for the same reason every other compat layer filters them.
/// `tools` non-empty → `functionDeclarations` declared natively and the
/// history carries real `functionCall`/`functionResponse` parts; empty →
/// byte-identical to the pre-native prompt-encoded body.
pub fn request_to_gemini(request: &CompletionRequest) -> Value {
    let (system_parts, turns) = build_contents(request);

    let contents: Vec<Value> = turns
        .into_iter()
        .map(|(role, parts)| {
            json!({
                "role": role,
                "parts": parts.iter().map(GeminiPart::to_json).collect::<Vec<_>>(),
            })
        })
        .collect();

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            "temperature": request.temperature,
            "maxOutputTokens": request.max_tokens,
        },
    });
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
    }
    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": sanitize_schema(&tool.parameters),
                })
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    body
}

// ── History building (native-aware) ──────────────────────────────────────

/// One `contents[].parts[]` entry, kept typed until serialization so plain
/// text can still merge with `"\n\n"` exactly like before native tools.
#[derive(Clone, Debug, PartialEq)]
enum GeminiPart {
    Text(String),
    /// Already-shaped `functionCall`/`functionResponse` object.
    Json(Value),
}

impl GeminiPart {
    fn to_json(&self) -> Value {
        match self {
            GeminiPart::Text(text) => json!({ "text": text }),
            GeminiPart::Json(value) => value.clone(),
        }
    }
}

fn is_singleton_text(parts: &[GeminiPart]) -> bool {
    matches!(parts, [GeminiPart::Text(_)])
}

/// Append `parts` as one message's contribution: merges into the previous
/// turn when the role matches (concatenating plain text as before, or
/// simply appending parts for a structured turn), else opens a new turn.
fn push_turn(
    turns: &mut Vec<(&'static str, Vec<GeminiPart>)>,
    role: &'static str,
    mut parts: Vec<GeminiPart>,
) {
    if parts.is_empty() {
        return;
    }
    match turns.last_mut() {
        Some((last_role, last_parts)) if *last_role == role => {
            if is_singleton_text(last_parts) && is_singleton_text(&parts) {
                if let (Some(GeminiPart::Text(existing)), GeminiPart::Text(new_text)) =
                    (last_parts.first_mut(), &parts[0])
                {
                    existing.push_str("\n\n");
                    existing.push_str(new_text);
                }
            } else {
                last_parts.append(&mut parts);
            }
        }
        _ => turns.push((role, parts)),
    }
}

/// Same shape as `compat_client::merge_alternating_turns`, but part-aware:
/// an assistant message with native `tool_calls` becomes a `model` turn with
/// `functionCall` parts, and a `Role::Tool` message answering an EARLIER
/// call becomes a `functionResponse` part in a `user` turn. Everything else
/// (plain text, unmatched tool results) merges exactly like the old
/// text-only path, so a request with no native calls comes out identical.
fn build_contents(
    request: &CompletionRequest,
) -> (Vec<&str>, Vec<(&'static str, Vec<GeminiPart>)>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut turns: Vec<(&'static str, Vec<GeminiPart>)> = Vec::new();
    // Вызов по нашему id — только вызовы БОЛЕЕ раннего сообщения.
    let mut declared: HashMap<&str, &ToolCall> = HashMap::new();

    for message in request.messages.iter().filter(|message| !message.ui_only) {
        match message.role {
            Role::System => {
                system_parts.push(&message.content);
            }
            Role::User => {
                if !message.content.is_empty() {
                    push_turn(
                        &mut turns,
                        "user",
                        vec![GeminiPart::Text(message.content.clone())],
                    );
                }
            }
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    if !message.content.is_empty() {
                        push_turn(
                            &mut turns,
                            "model",
                            vec![GeminiPart::Text(message.content.clone())],
                        );
                    }
                } else {
                    let mut parts = Vec::with_capacity(message.tool_calls.len() + 1);
                    if !message.content.is_empty() {
                        parts.push(GeminiPart::Text(message.content.clone()));
                    }
                    for call in &message.tool_calls {
                        declared.insert(call.id.as_str(), call);
                        parts.push(GeminiPart::Json(function_call_part(call)));
                    }
                    push_turn(&mut turns, "model", parts);
                }
            }
            Role::Tool => {
                let matched = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| declared.get(id).copied());
                match matched {
                    Some(call) => {
                        let response = if message.tool_error {
                            json!({ "error": message.content })
                        } else {
                            json!({ "content": message.content })
                        };
                        let mut function_response =
                            json!({ "name": call.name, "response": response });
                        // Gemini 3 сверяет ответ с вызовом по его id.
                        if let Some(id) = gemini_state(call, "id") {
                            function_response["id"] = id.clone();
                        }
                        push_turn(
                            &mut turns,
                            "user",
                            vec![GeminiPart::Json(
                                json!({ "functionResponse": function_response }),
                            )],
                        );
                    }
                    None => {
                        // Прежнее поведение промптового пути: результат без
                        // объявившего его вызова уходит обычным текстом.
                        if !message.content.is_empty() {
                            let text = format!("[tool result]\n{}", message.content);
                            push_turn(&mut turns, "user", vec![GeminiPart::Text(text)]);
                        }
                    }
                }
            }
        }
    }

    if turns.first().is_none_or(|(role, _)| *role == "model") {
        turns.insert(
            0,
            ("user", vec![GeminiPart::Text("(continue)".to_string())]),
        );
    }

    (system_parts, turns)
}

/// Часть `functionCall` из истории. Свой id наружу не уходит — только тот, что
/// прислал Gemini; `thoughtSignature` возвращается рядом, иначе Gemini 3
/// теряет ход рассуждений.
fn function_call_part(call: &ToolCall) -> Value {
    let mut function_call = json!({ "name": call.name, "args": call.arguments });
    if let Some(id) = gemini_state(call, "id") {
        function_call["id"] = id.clone();
    }
    let mut part = json!({ "functionCall": function_call });
    if let Some(signature) = gemini_state(call, "thoughtSignature") {
        part["thoughtSignature"] = signature.clone();
    }
    part
}

/// Поле из `ToolCall::provider_state`, которое Gemini прислал с вызовом.
fn gemini_state<'c>(call: &'c ToolCall, key: &str) -> Option<&'c Value> {
    call.provider_state.as_ref()?.get(key)
}

// ── Schema sanitizing (functionDeclarations.parameters) ─────────────────

/// JSON-Schema-only keys outside the OpenAPI subset Gemini's function
/// calling accepts. MCP servers hand us full JSON Schema (often with
/// `$schema`/`additionalProperties`), which the plain `functionDeclarations`
/// converter can reject — strip them recursively before sending.
const UNSUPPORTED_SCHEMA_KEYS: [&str; 3] = ["$schema", "$id", "additionalProperties"];

/// Recursively drop [`UNSUPPORTED_SCHEMA_KEYS`] from a JSON Schema value.
fn sanitize_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut cleaned = Map::with_capacity(map.len());
            for (key, val) in map {
                if UNSUPPORTED_SCHEMA_KEYS.contains(&key.as_str()) {
                    continue;
                }
                cleaned.insert(key.clone(), sanitize_schema(val));
            }
            Value::Object(cleaned)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_schema).collect()),
        other => other.clone(),
    }
}

// ── Response (shared by non-streaming AND each streamed SSE chunk) ──────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Content {
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
pub struct Part {
    #[serde(default)]
    pub text: Option<String>,
    /// Приходит целиком, не по кускам — см. доккомментарий модуля.
    #[serde(default, rename = "functionCall")]
    pub function_call: Option<FunctionCallPart>,
    /// Подпись рассуждения при вызове: её надо вернуть в истории как есть.
    #[serde(default, rename = "thoughtSignature")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCallPart {
    /// Обычно его нет; если API всё же прислал — используем его, а не свой.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
}

/// Map Gemini finish reasons onto the OpenAI-style vocabulary the rest of
/// the app speaks; unknown reasons pass through lowercased. `STOP` becomes
/// `tool_calls` when the candidate actually carried native calls, matching
/// the vocabulary the agent loop treats as a normal (non-truncated) end.
fn map_finish_reason(reason: &str, has_calls: bool) -> String {
    match reason {
        "STOP" if has_calls => "tool_calls".to_string(),
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        other => other.to_lowercase(),
    }
}

/// First-candidate text + collected native tool calls + mapped finish
/// reason — the shared extractor for the non-streaming response and every
/// streamed chunk. Calls arrive whole in one `functionCall` part, never
/// split across chunks.
pub fn extract_piece(
    response: &GenerateContentResponse,
) -> (String, Vec<ToolCall>, Option<String>) {
    let Some(candidate) = response.candidates.first() else {
        return (String::new(), Vec::new(), None);
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    if let Some(content) = &candidate.content {
        for part in &content.parts {
            if let Some(part_text) = &part.text {
                text.push_str(part_text);
            }
            if let Some(call) = &part.function_call {
                let id = call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                let arguments = if call.args.is_null() {
                    json!({})
                } else {
                    call.args.clone()
                };
                let mut state = serde_json::Map::new();
                if let Some(gemini_id) = &call.id {
                    state.insert("id".to_string(), json!(gemini_id));
                }
                if let Some(signature) = &part.thought_signature {
                    state.insert("thoughtSignature".to_string(), json!(signature));
                }
                calls.push(ToolCall {
                    id,
                    name: call.name.clone(),
                    arguments,
                    provider_state: (!state.is_empty()).then_some(Value::Object(state)),
                });
            }
        }
    }
    let finish = candidate
        .finish_reason
        .as_deref()
        .map(|reason| map_finish_reason(reason, !calls.is_empty()));
    (text, calls, finish)
}

pub fn response_from_gemini(response: GenerateContentResponse) -> CompletionResponse {
    // `CompletionResponse` has no `tool_calls` field (shared with every other
    // compat provider — see provider/mod.rs); native calls only ride through
    // the streaming `CompletionChunk` path today.
    let (content, _calls, finish_reason) = extract_piece(&response);
    CompletionResponse {
        content,
        finish_reason,
        usage: response.usage_metadata.map(|usage| Usage {
            prompt_tokens: usage.prompt_token_count,
            completion_tokens: usage.candidates_token_count,
            total_tokens: usage.total_token_count,
        }),
    }
}

// ── Models listing ──────────────────────────────────────────────────────

/// `GET {base}/models` page. Names come back as `models/<id>` — the
/// transport strips the prefix so `/models` shows bare ids.
#[derive(Debug, Deserialize)]
pub struct ModelsPage {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    fn base_request(
        messages: Vec<ChatMessage>,
        tools: Vec<crate::tools::ToolDefinition>,
    ) -> CompletionRequest {
        CompletionRequest {
            messages,
            tools,
            model: "gemini-2.5-flash".to_string(),
            temperature: 0.4,
            max_tokens: 512,
            stream: false,
        }
    }

    /// Gemini 3: id вызова и `thoughtSignature` из ответа возвращаются в истории
    /// как есть — и id доходит до `functionResponse`.
    #[test]
    fn gemini_call_id_and_thought_signature_round_trip() {
        let chunk: GenerateContentResponse = serde_json::from_value(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{
                    "functionCall": {"id": "g1", "name": "read", "args": {"path": "a"}},
                    "thoughtSignature": "sig=="
                }]},
                "finishReason": "STOP"
            }]
        }))
        .unwrap();
        let (_, calls, finish) = extract_piece(&chunk);
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        let call = calls[0].clone();
        assert_eq!(call.id, "g1");

        let mut assistant = ChatMessage::assistant("");
        assistant.tool_calls = vec![call];
        let request = base_request(
            vec![
                ChatMessage::user("go"),
                assistant,
                ChatMessage::tool("g1", "text"),
            ],
            Vec::new(),
        );
        let body = request_to_gemini(&request);
        let contents = body["contents"].as_array().unwrap();
        let model_part = &contents[1]["parts"][0];
        assert_eq!(model_part["functionCall"]["id"], "g1");
        assert_eq!(model_part["thoughtSignature"], "sig==");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["id"], "g1");
    }

    #[test]
    fn system_instruction_and_alternating_contents() {
        let request = base_request(
            vec![
                ChatMessage::system("be terse"),
                ChatMessage::user("hi"),
                ChatMessage::tool("call_1", "42"),
                ChatMessage::assistant("ok"),
            ],
            Vec::new(),
        );
        let body = request_to_gemini(&request);

        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
        let contents = body["contents"].as_array().unwrap();
        // user + tool merged; assistant → "model".
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert!(
            contents[0]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("[tool result]")
        );
        assert_eq!(contents[1]["role"], "model");
    }

    #[test]
    fn model_first_history_gets_user_opener() {
        let request = CompletionRequest {
            messages: vec![ChatMessage::assistant("earlier answer")],
            tools: Vec::new(),
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 16,
            stream: true,
        };
        let body = request_to_gemini(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
    }

    #[test]
    fn response_extracts_text_finish_and_usage() {
        let raw = r#"{
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hel"}, {"text": "lo"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10}
        }"#;
        let parsed: GenerateContentResponse = serde_json::from_str(raw).unwrap();
        let internal = response_from_gemini(parsed);
        assert_eq!(internal.content, "Hello");
        assert_eq!(internal.finish_reason.as_deref(), Some("stop"));
        assert_eq!(internal.usage.unwrap().total_tokens, 10);
    }

    #[test]
    fn finish_reasons_map() {
        let parsed: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates": [{"finishReason": "MAX_TOKENS"}]}"#).unwrap();
        assert_eq!(extract_piece(&parsed).2.as_deref(), Some("length"));

        let parsed: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates": [{"finishReason": "SAFETY"}]}"#).unwrap();
        assert_eq!(extract_piece(&parsed).2.as_deref(), Some("safety"));
    }

    #[test]
    fn stream_chunk_without_finish_is_plain_text() {
        // A mid-stream chunk is the same shape, just no finishReason.
        let parsed: GenerateContentResponse =
            serde_json::from_str(r#"{"candidates": [{"content": {"parts": [{"text": "tok"}]}}]}"#)
                .unwrap();
        let (text, calls, finish) = extract_piece(&parsed);
        assert_eq!(text, "tok");
        assert!(calls.is_empty());
        assert!(finish.is_none());
    }

    #[test]
    fn no_tools_field_appears_when_none_are_declared() {
        let request = base_request(vec![ChatMessage::user("hi")], Vec::new());
        let body = request_to_gemini(&request);
        assert!(body.get("tools").is_none(), "{body}");
    }

    #[test]
    fn declared_tools_emit_function_declarations() {
        let request = base_request(
            vec![ChatMessage::user("hi")],
            vec![crate::tools::ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
        );
        let body = request_to_gemini(&request);
        let decls = body["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "read_file");
        assert_eq!(decls[0]["description"], "Read a file");
        assert_eq!(decls[0]["parameters"]["type"], "object");
    }

    /// Full native round: assistant text + 2 calls, then 2 results, then a
    /// follow-up user turn — parts and role alternation must come out right.
    #[test]
    fn native_round_trip_builds_function_call_and_response_parts() {
        let mut assistant = ChatMessage::assistant("Let me check both.");
        assistant.tool_calls = vec![
            ToolCall {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                arguments: json!({"city": "Boston"}),
                provider_state: None,
            },
            ToolCall {
                id: "call_2".to_string(),
                name: "get_time".to_string(),
                arguments: json!({"city": "Boston"}),
                provider_state: None,
            },
        ];
        let result_1 = ChatMessage::tool_with_display("call_1", "get_weather", "52F", "52F", false);
        let result_2 = ChatMessage::tool_with_display("call_2", "get_time", "boom", "boom", true);

        let request = base_request(
            vec![
                ChatMessage::user("What's the weather and time in Boston?"),
                assistant,
                result_1,
                result_2,
                ChatMessage::user("thanks"),
            ],
            vec![crate::tools::ToolDefinition {
                name: "get_weather".to_string(),
                description: "d".to_string(),
                parameters: json!({"type": "object"}),
            }],
        );
        let body = request_to_gemini(&request);
        let contents = body["contents"].as_array().unwrap();

        // user, model(text+2 calls), user(2 functionResponse), user(merged "thanks")
        // — the trailing plain user text merges into the functionResponse turn.
        assert_eq!(contents.len(), 3, "{contents:#?}");
        assert_eq!(contents[0]["role"], "user");

        assert_eq!(contents[1]["role"], "model");
        let model_parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(model_parts.len(), 3);
        assert_eq!(model_parts[0]["text"], "Let me check both.");
        assert_eq!(model_parts[1]["functionCall"]["name"], "get_weather");
        assert_eq!(model_parts[1]["functionCall"]["args"]["city"], "Boston");
        assert!(
            model_parts[1].get("id").is_none(),
            "id never rides the wire"
        );
        assert_eq!(model_parts[2]["functionCall"]["name"], "get_time");

        assert_eq!(contents[2]["role"], "user");
        let reply_parts = contents[2]["parts"].as_array().unwrap();
        assert_eq!(reply_parts.len(), 3);
        assert_eq!(reply_parts[0]["functionResponse"]["name"], "get_weather");
        assert_eq!(
            reply_parts[0]["functionResponse"]["response"]["content"],
            "52F"
        );
        assert_eq!(reply_parts[1]["functionResponse"]["name"], "get_time");
        assert_eq!(
            reply_parts[1]["functionResponse"]["response"]["error"],
            "boom"
        );
        assert_eq!(reply_parts[2]["text"], "thanks");
    }

    /// A tool message whose id matches no earlier assistant call (the
    /// prompt path, or a stale id) still goes out as plain user text.
    #[test]
    fn unmatched_tool_message_stays_text() {
        let request = base_request(
            vec![
                ChatMessage::user("hi"),
                ChatMessage::tool("call_orphan", "42"),
            ],
            Vec::new(),
        );
        let body = request_to_gemini(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        // Промптовый путь склеивает подряд идущие реплики пользователя в одну часть.
        let text = contents[0]["parts"][0]["text"].as_str().unwrap_or_default();
        assert!(
            text.contains("hi") && text.contains("[tool result]"),
            "{text}"
        );
    }

    #[test]
    fn sanitize_schema_strips_unsupported_keys_recursively() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "nested": {
                    "$id": "#nested",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "x": { "type": "string" } }
                }
            }
        });
        let cleaned = sanitize_schema(&schema);
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "object");
        let nested = &cleaned["properties"]["nested"];
        assert!(nested.get("$id").is_none());
        assert!(nested.get("additionalProperties").is_none());
        assert_eq!(nested["properties"]["x"]["type"], "string");
    }
}
