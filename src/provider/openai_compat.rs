//! OpenAI-compatible wire format ⇄ internal provider types.
//!
//! This is the protocol boundary for two features that share one
//! vocabulary:
//!
//! 1. **Server mode** (`crate::server`): pooprusteek exposes
//!    `POST /v1/chat/completions` (+ `GET /v1/models`) so existing
//!    OpenAI-speaking tools can use it as a backend while it internally
//!    talks to whatever providers it has. Inbound direction:
//!    [`ChatCompletionRequest`] → [`to_internal_request`] → run → outbound
//!    via [`response_to_openai`] / [`delta_chunk`]+[`final_chunk`].
//! 2. **OpenAI-compatible client providers**
//!    (`crate::provider::openai_client`): the same types in the opposite
//!    direction ([`request_to_openai`], [`response_from_openai`],
//!    [`chunk_from_openai`]).
//!
//! Scope (v1): plain chat — messages, temperature/max_tokens, streaming,
//! finish_reason, usage. Deliberately NOT translated yet: structured
//! tool-calling (`tools`/`tool_calls`) — pooprusteek's tool calls are
//! prompt-encoded text parsed by `agent/tool_parser`, and mapping that onto
//! OpenAI's structured tool protocol is its own design task. Inbound
//! `tools` are captured (not dropped by serde) so the server can *warn*
//! rather than silently ignore; multimodal content parts are flattened to
//! their text parts.
//!
//! Not translating tool-calling still has to mean *tolerating* it on the
//! wire, in both directions. Inbound: an assistant message whose whole point
//! is its `tool_calls` carries `"content": null`, which must parse rather
//! than 400 the request. Outbound: a `Role::Tool` message goes out as user
//! text, because our assistant messages carry no `tool_calls` for it to
//! answer and a lone `role:"tool"` is rejected by strict endpoints.
//!
//! Everything here is pure data mapping — no I/O, no `App`, no network.

use crate::provider::{
    ChatMessage, CompletionChunk, CompletionRequest, CompletionResponse, Role, Usage,
    estimate_tokens,
};
use serde::{Deserialize, Serialize};

// ── Wire types: request ────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatCompletionMessage>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Legacy name, still what most tools send.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Current OpenAI name; wins over `max_tokens` when both are present.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
    /// Captured (not silently swallowed by serde) so a server can tell the
    /// caller that structured tool-calling is not translated yet.
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionMessage {
    pub role: String,
    /// Отсутствующее поле читается как `null`: у ассистентского сообщения с
    /// одними вызовами инструментов текста нет вовсе.
    #[serde(default)]
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// DeepSeek/OpenAI reasoning-model field: the model's chain-of-thought,
    /// kept out of `content`. Populated by [`split_reasoning`] when the
    /// backend emitted a leading `<think>`/`<thinking>` block inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

/// OpenAI message content: a plain string, or an array of typed parts
/// (multimodal). Non-text parts are dropped at conversion — text-only
/// backend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
    /// `"content": null` — что OpenAI шлёт в ассистентском сообщении, чей
    /// смысл целиком в `tool_calls`. Без этого варианта разбор всего тела
    /// падал, и клиент, проигрывающий свою же историю вызовов, получал от
    /// нашего сервера 400 ещё на входе.
    #[default]
    Null,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl MessageContent {
    /// Flatten to plain text: string as-is; part arrays joined from their
    /// `text` fields (image/audio parts contribute nothing).
    pub fn into_text(self) -> String {
        match self {
            MessageContent::Text(text) => text,
            MessageContent::Parts(parts) => parts
                .into_iter()
                .filter_map(|part| part.text)
                .collect::<Vec<_>>()
                .join("\n"),
            MessageContent::Null => String::new(),
        }
    }
}

// ── Wire types: response / chunk / models / error ──────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    /// Always `"chat.completion"`.
    pub object: String,
    /// Unix seconds.
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatCompletionMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    /// Always `"chat.completion.chunk"`.
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

/// Streaming delta. Per the OpenAI protocol the first chunk of a turn
/// carries `role: "assistant"`, content chunks carry `content`, and the
/// terminal chunk carries neither — only a `finish_reason` on its choice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Streaming half of [`ChatCompletionMessage::reasoning_content`] — the
    /// live chain-of-thought deltas, routed here by [`ReasoningStreamSplitter`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelList {
    /// Always `"list"`.
    pub object: String,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// Always `"model"`.
    pub object: String,
    pub created: u64,
    pub owned_by: String,
}

/// The OpenAI error envelope (`{"error": {...}}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorResponse {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                message: message.into(),
                kind: "invalid_request_error".to_string(),
                code: None,
            },
        }
    }

    pub fn server_error(message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                message: message.into(),
                kind: "server_error".to_string(),
                code: None,
            },
        }
    }
}

// ── Completion identity ────────────────────────────────────────────────

/// The `id`/`created`/`model` triple stamped onto every response and every
/// chunk of one completion. Generated once per turn so all chunks of a
/// stream agree, and injectable in tests.
#[derive(Debug, Clone)]
pub struct CompletionMeta {
    pub id: String,
    pub created: u64,
    pub model: String,
}

impl CompletionMeta {
    pub fn generate(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            created: chrono::Utc::now().timestamp().max(0) as u64,
            model: model.to_string(),
        }
    }
}

// ── Defaults for fields the wire request may omit ──────────────────────

/// Fill-ins for optional request fields, supplied by the embedding side
/// (the server will feed these from `Config`).
#[derive(Debug, Clone)]
pub struct RequestDefaults {
    pub temperature: f32,
    pub max_tokens: u32,
}

// ── Inbound: OpenAI request → internal ─────────────────────────────────

fn role_from_openai(role: &str) -> Result<Role, String> {
    match role {
        // "developer" is OpenAI's newer alias for the system role.
        "system" | "developer" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        // "function" is the pre-tools deprecated spelling of "tool".
        "tool" | "function" => Ok(Role::Tool),
        other => Err(format!("unsupported message role: {other:?}")),
    }
}

fn role_to_openai(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Convert an inbound OpenAI-format request into the internal
/// [`CompletionRequest`]. Fails (with an `ErrorResponse`-ready message) on
/// roles the internal model has no equivalent for.
pub fn to_internal_request(
    request: ChatCompletionRequest,
    defaults: &RequestDefaults,
) -> Result<CompletionRequest, String> {
    let stream = request.stream.unwrap_or(false);
    let mut messages = Vec::with_capacity(request.messages.len());
    for message in request.messages {
        messages.push(message_to_internal(message)?);
    }
    Ok(CompletionRequest {
        messages,
        model: request.model,
        temperature: request.temperature.unwrap_or(defaults.temperature),
        max_tokens: request
            .max_completion_tokens
            .or(request.max_tokens)
            .unwrap_or(defaults.max_tokens),
        stream,
    })
}

fn message_to_internal(message: ChatCompletionMessage) -> Result<ChatMessage, String> {
    let role = role_from_openai(&message.role)?;
    let text = message.content.into_text();
    let mut converted = match role {
        Role::System => ChatMessage::system(&text),
        Role::User => ChatMessage::user(&text),
        Role::Assistant => ChatMessage::assistant(&text),
        Role::Tool => ChatMessage::tool(message.tool_call_id.as_deref().unwrap_or_default(), &text),
    };
    converted.name = message.name;
    Ok(converted)
}

// ── Outbound: internal → OpenAI request (future client providers) ──────

/// Convert an internal request into the OpenAI wire shape — the sending
/// half of a future OpenAI-compatible `LLMProvider`. `ui_only` messages
/// are filtered out here for the same reason `provider/prompt.rs` filters
/// them: UI chrome must never reach a model.
pub fn request_to_openai(request: &CompletionRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .filter(|message| !message.ui_only)
        .map(|message| {
            // Результат инструмента уходит текстом от пользователя — ровно
            // как в anthropic_compat и gemini_compat. Протокол вызовов у нас
            // промптовый, у ассистента нет `tool_calls`, а `role:"tool"` без
            // них строгие эндпоинты (api.openai.com и шлюзы) отвергают с 400:
            // «messages with role 'tool' must be a response to a preceeding
            // message with 'tool_calls'». Локальные серверы это прощали,
            // поэтому баг дожил досюда.
            if message.role == Role::Tool {
                let body = match &message.name {
                    Some(name) => format!("[tool result: {name}]\n{}", message.content),
                    None => format!("[tool result]\n{}", message.content),
                };
                return serde_json::json!({ "role": "user", "content": body });
            }
            let mut object = serde_json::json!({
                "role": role_to_openai(&message.role),
                "content": message.content,
            });
            if let Some(name) = &message.name {
                object["name"] = serde_json::json!(name);
            }
            object
        })
        .collect();
    serde_json::json!({
        "model": request.model,
        "messages": messages,
        "temperature": request.temperature,
        "max_tokens": request.max_tokens,
        "stream": request.stream,
    })
}

// ── Outbound: internal result → OpenAI response / chunks ───────────────

/// Usage with estimated counts (the DeepSeek web API never reports real
/// ones — see STATE.md KNOWN GAPS) — same `estimate_tokens` heuristic the
/// TUI stats use. Real usage, when a provider has it, should be passed to
/// [`response_to_openai`] instead.
pub fn estimated_usage(prompt_texts: &[&str], completion: &str) -> Usage {
    let prompt_tokens: u32 = prompt_texts.iter().map(|text| estimate_tokens(text)).sum();
    let completion_tokens = estimate_tokens(completion);
    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    }
}

/// `fallback_usage` (usually [`estimated_usage`]) is reported only when the
/// provider didn't supply real numbers of its own.
pub fn response_to_openai(
    response: &CompletionResponse,
    fallback_usage: Usage,
    meta: &CompletionMeta,
) -> ChatCompletionResponse {
    // Reasoning models (deepseek-r1, qwen3, gpt-oss, … via any backend that
    // emits inline `<think>`) fold chain-of-thought into content; hoist it
    // into the OpenAI `reasoning_content` field so clients get a clean answer.
    let (reasoning, content) = split_reasoning(&response.content);
    ChatCompletionResponse {
        id: meta.id.clone(),
        object: "chat.completion".to_string(),
        created: meta.created,
        model: meta.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: ChatCompletionMessage {
                role: "assistant".to_string(),
                content: MessageContent::Text(content),
                name: None,
                tool_call_id: None,
                reasoning_content: reasoning,
            },
            finish_reason: Some(
                response
                    .finish_reason
                    .clone()
                    .unwrap_or_else(|| "stop".to_string()),
            ),
        }],
        usage: response.usage.clone().unwrap_or(fallback_usage),
    }
}

/// A streamed delta carrying reasoning and/or content deltas (either may be
/// empty). `first` announces `role: "assistant"` per the OpenAI protocol.
pub fn split_delta_chunk(
    reasoning: &str,
    content: &str,
    first: bool,
    meta: &CompletionMeta,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: meta.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: meta.created,
        model: meta.model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: Delta {
                role: first.then(|| "assistant".to_string()),
                content: (!content.is_empty()).then(|| content.to_string()),
                reasoning_content: (!reasoning.is_empty()).then(|| reasoning.to_string()),
            },
            finish_reason: None,
        }],
    }
}

/// The terminal chunk: empty delta, `finish_reason` set. The transport
/// layer follows it with the literal `data: [DONE]` SSE line.
pub fn final_chunk(finish_reason: &str, meta: &CompletionMeta) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: meta.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: meta.created,
        model: meta.model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: Delta::default(),
            finish_reason: Some(finish_reason.to_string()),
        }],
    }
}

// ── Inbound: OpenAI response / chunk → internal (future client) ────────

/// First-choice extraction for a future OpenAI-compatible client provider.
pub fn response_from_openai(response: ChatCompletionResponse) -> CompletionResponse {
    let (content, finish_reason) = response
        .choices
        .into_iter()
        .next()
        .map(|choice| (choice.message.content.into_text(), choice.finish_reason))
        .unwrap_or_default();
    CompletionResponse {
        content,
        finish_reason,
        usage: Some(response.usage),
    }
}

pub fn chunk_from_openai(chunk: ChatCompletionChunk) -> CompletionChunk {
    let (content, finish_reason) = chunk
        .choices
        .into_iter()
        .next()
        .map(|choice| {
            (
                choice.delta.content.unwrap_or_default(),
                choice.finish_reason,
            )
        })
        .unwrap_or_default();
    CompletionChunk {
        content,
        finish_reason,
    }
}

// ── Reasoning extraction (`<think>` → `reasoning_content`) ──────────────
//
// Reasoning models (deepseek-r1, qwen3, gpt-oss, … through Ollama/vLLM/LM
// Studio) prepend their chain-of-thought to `content` wrapped in a
// `<think>…</think>` (or `<thinking>…</thinking>`) block — the same
// convention `agent/tool_parser` already strips for the TUI. The server
// hoists that block into OpenAI's `reasoning_content` field so API clients
// get a clean final answer. Only a block at the very START of the content
// is treated as reasoning (that is where these models put it), so a normal
// answer that happens to mention `<think>` mid-text is untouched.
//
// NOTE: the built-in DeepSeek *web* reasoner is a separate case — it streams
// typed THINK fragments the provider currently merges into content with no
// tags, so there is nothing here to split. Separating those cleanly needs a
// reasoning channel on `CompletionChunk` + fragment-type tracking in
// `provider/deepseek/stream.rs` (and it would change the TUI's inline-thinking
// display) — left as a deliberate follow-up.

const THINK_OPENERS: [&str; 2] = ["<think>", "<thinking>"];
const THINK_CLOSERS: [&str; 2] = ["</think>", "</thinking>"];

/// Split a completed message into `(reasoning, content)`. Extracts a leading
/// `<think>…</think>` block; an unterminated opener (model hit the token
/// budget mid-thought) makes the whole remainder reasoning with empty
/// content. No leading think block → `(None, content unchanged)`.
pub fn split_reasoning(content: &str) -> (Option<String>, String) {
    let trimmed = content.trim_start();
    let Some(opener) = THINK_OPENERS
        .iter()
        .find(|open| trimmed.starts_with(**open))
    else {
        return (None, content.to_string());
    };
    let after_open = &trimmed[opener.len()..];
    let close = THINK_CLOSERS
        .iter()
        .filter_map(|close| after_open.find(close).map(|index| (index, close.len())))
        .min_by_key(|(index, _)| *index);
    let (reasoning, rest) = match close {
        Some((index, close_len)) => (
            after_open[..index].trim().to_string(),
            after_open[index + close_len..].trim_start().to_string(),
        ),
        // Unterminated (e.g. finish_reason == "length"): all reasoning.
        None => (after_open.trim().to_string(), String::new()),
    };
    ((!reasoning.is_empty()).then_some(reasoning), rest)
}

/// Longest byte length `k` such that `buffer`'s last `k` bytes form a
/// non-empty prefix of some `token` (and land on a char boundary) — the
/// tail a streaming splitter must hold back in case a tag is split across
/// deltas. `0` when no suffix could extend into a token.
fn holdback_len(buffer: &str, tokens: &[&str]) -> usize {
    let max = tokens
        .iter()
        .map(|token| token.len())
        .max()
        .unwrap_or(0)
        .saturating_sub(1);
    for k in (1..=max.min(buffer.len())).rev() {
        let start = buffer.len() - k;
        if !buffer.is_char_boundary(start) {
            continue;
        }
        let suffix = &buffer[start..];
        if tokens.iter().any(|token| token.starts_with(suffix)) {
            return k;
        }
    }
    0
}

#[derive(Debug, PartialEq)]
enum SplitPhase {
    /// Deciding whether the stream opens with a think block.
    Start,
    /// Inside a `<think>` block, watching for the closer.
    InReasoning,
    /// Past any think block — everything is content.
    InContent,
}

/// Incremental [`split_reasoning`] for the streaming path: feed content
/// deltas, get back `(reasoning_delta, content_delta)`, then [`finish`] to
/// flush any held tail. Holds back only a partial-tag-sized suffix, so
/// normal (non-reasoning) output flows through essentially unbuffered.
///
/// [`finish`]: ReasoningStreamSplitter::finish
pub struct ReasoningStreamSplitter {
    phase: SplitPhase,
    buffer: String,
}

impl Default for ReasoningStreamSplitter {
    fn default() -> Self {
        Self {
            phase: SplitPhase::Start,
            buffer: String::new(),
        }
    }
}

impl ReasoningStreamSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify a content delta. Returns `(reasoning, content)` produced by
    /// this push (either may be empty).
    pub fn push(&mut self, delta: &str) -> (String, String) {
        self.buffer.push_str(delta);
        let mut reasoning = String::new();
        let mut content = String::new();
        loop {
            match self.phase {
                SplitPhase::Start => {
                    let trimmed = self.buffer.trim_start();
                    if let Some(opener) = THINK_OPENERS
                        .iter()
                        .find(|open| trimmed.starts_with(**open))
                    {
                        // Drop the leading whitespace + opener; the rest is
                        // reasoning to classify next iteration.
                        let remainder = trimmed[opener.len()..].to_string();
                        self.buffer = remainder;
                        self.phase = SplitPhase::InReasoning;
                        continue;
                    }
                    // Still possibly growing into an opener → hold everything.
                    if THINK_OPENERS.iter().any(|open| open.starts_with(trimmed)) {
                        break;
                    }
                    // Not a think block: flush all as content, for good.
                    content.push_str(&self.buffer);
                    self.buffer.clear();
                    self.phase = SplitPhase::InContent;
                    break;
                }
                SplitPhase::InReasoning => {
                    let closer = THINK_CLOSERS
                        .iter()
                        .filter_map(|close| self.buffer.find(close).map(|i| (i, close.len())))
                        .min_by_key(|(i, _)| *i);
                    if let Some((index, close_len)) = closer {
                        reasoning.push_str(&self.buffer[..index]);
                        self.buffer = self.buffer[index + close_len..].to_string();
                        self.phase = SplitPhase::InContent;
                        continue;
                    }
                    // No full closer yet — emit all but a possible partial one.
                    let hold = holdback_len(&self.buffer, &THINK_CLOSERS);
                    let emit_to = self.buffer.len() - hold;
                    reasoning.push_str(&self.buffer[..emit_to]);
                    self.buffer = self.buffer[emit_to..].to_string();
                    break;
                }
                SplitPhase::InContent => {
                    content.push_str(&self.buffer);
                    self.buffer.clear();
                    break;
                }
            }
        }
        (reasoning, content)
    }

    /// Flush the held tail at end of stream. A never-closed opener yields
    /// trailing reasoning; anything else is content.
    pub fn finish(self) -> (String, String) {
        match self.phase {
            SplitPhase::InReasoning => (self.buffer, String::new()),
            _ => (String::new(), self.buffer),
        }
    }
}

// ── Models listing ──────────────────────────────────────────────────────

/// `GET /v1/models` payload for a fixed model catalog.
pub fn model_list(ids: &[&str], owned_by: &str, created: u64) -> ModelList {
    ModelList {
        object: "list".to_string(),
        data: ids
            .iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                object: "model".to_string(),
                created,
                owned_by: owned_by.to_string(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> RequestDefaults {
        RequestDefaults {
            temperature: 0.7,
            max_tokens: 4096,
        }
    }

    fn meta() -> CompletionMeta {
        CompletionMeta {
            id: "chatcmpl-test".to_string(),
            created: 1_700_000_000,
            model: "deepseek-chat".to_string(),
        }
    }

    #[test]
    fn request_with_string_content_parses_and_converts() {
        let raw = r#"{
            "model": "deepseek-chat",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hi"}
            ],
            "stream": true,
            "some_future_field": {"ignored": true}
        }"#;
        let request: ChatCompletionRequest = serde_json::from_str(raw).unwrap();
        let internal = to_internal_request(request, &defaults()).unwrap();

        assert_eq!(internal.model, "deepseek-chat");
        assert_eq!(internal.messages.len(), 2);
        assert_eq!(internal.messages[0].role, Role::System);
        assert_eq!(internal.messages[0].content, "be terse");
        assert_eq!(internal.messages[1].role, Role::User);
        // Omitted optionals fall back to the embedder's defaults.
        assert_eq!(internal.temperature, 0.7);
        assert_eq!(internal.max_tokens, 4096);
    }

    #[test]
    fn content_parts_flatten_to_text_only() {
        let raw = r#"{
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look at this"},
                {"type": "image_url", "image_url": {"url": "data:..."}},
                {"type": "text", "text": "and this"}
            ]}]
        }"#;
        let request: ChatCompletionRequest = serde_json::from_str(raw).unwrap();
        let internal = to_internal_request(request, &defaults()).unwrap();
        assert_eq!(internal.messages[0].content, "look at this\nand this");
    }

    #[test]
    fn role_aliases_and_unknown_roles() {
        assert_eq!(role_from_openai("developer").unwrap(), Role::System);
        assert_eq!(role_from_openai("function").unwrap(), Role::Tool);
        assert!(role_from_openai("robot").is_err());
    }

    #[test]
    fn max_completion_tokens_wins_over_legacy_max_tokens() {
        let raw = r#"{
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "max_tokens": 100,
            "max_completion_tokens": 200
        }"#;
        let request: ChatCompletionRequest = serde_json::from_str(raw).unwrap();
        let internal = to_internal_request(request, &defaults()).unwrap();
        assert_eq!(internal.max_tokens, 200);
    }

    #[test]
    fn response_serializes_with_openai_shape() {
        let response = CompletionResponse {
            content: "hello".to_string(),
            finish_reason: None,
            usage: None,
        };
        let openai = response_to_openai(&response, estimated_usage(&["hi"], "hello"), &meta());
        let json = serde_json::to_value(&openai).unwrap();

        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["id"], "chatcmpl-test");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["choices"][0]["message"]["content"], "hello");
        // finish_reason defaults to "stop" when the provider reported none.
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert!(json["usage"]["total_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn stream_protocol_first_delta_and_final_chunk() {
        let first = split_delta_chunk("", "Hel", true, &meta());
        let middle = split_delta_chunk("", "lo", false, &meta());
        let last = final_chunk("stop", &meta());

        let first_json = serde_json::to_value(&first).unwrap();
        assert_eq!(first_json["object"], "chat.completion.chunk");
        assert_eq!(first_json["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(first_json["choices"][0]["delta"]["content"], "Hel");

        let middle_json = serde_json::to_value(&middle).unwrap();
        assert!(middle_json["choices"][0]["delta"].get("role").is_none());

        let last_json = serde_json::to_value(&last).unwrap();
        assert_eq!(last_json["choices"][0]["finish_reason"], "stop");
        assert!(last_json["choices"][0]["delta"].get("content").is_none());

        // All chunks of one completion share the same id.
        assert_eq!(first.id, last.id);
    }

    #[test]
    fn internal_request_round_trips_through_openai_shape() {
        let mut ui_notice = ChatMessage::system("ui chrome");
        ui_notice.ui_only = true;
        let internal = CompletionRequest {
            messages: vec![
                ChatMessage::system("sys"),
                ui_notice,
                ChatMessage::user("question"),
                ChatMessage::tool("call_1", "result"),
            ],
            model: "deepseek-chat".to_string(),
            temperature: 0.3,
            max_tokens: 512,
            stream: true,
        };

        let wire = request_to_openai(&internal);
        // ui_only chrome must not survive the conversion.
        assert_eq!(wire["messages"].as_array().unwrap().len(), 3);
        // Результат инструмента — пользовательский текст, а не `role:"tool"`:
        // см. комментарий в `request_to_openai`.
        assert_eq!(wire["messages"][2]["role"], "user");
        assert!(
            wire["messages"][2]["content"]
                .as_str()
                .unwrap()
                .contains("result")
        );
        assert!(
            wire["messages"][2].get("tool_call_id").is_none(),
            "an orphan tool_call_id is exactly what strict endpoints reject"
        );

        // And the wire shape parses back through the inbound path.
        let raw = serde_json::json!({
            "model": wire["model"],
            "messages": wire["messages"],
            "temperature": wire["temperature"],
            "max_tokens": wire["max_tokens"],
        });
        let reparsed: ChatCompletionRequest = serde_json::from_value(raw).unwrap();
        let back = to_internal_request(reparsed, &defaults()).unwrap();
        assert_eq!(back.messages.len(), 3);
        assert_eq!(back.messages[0].role, Role::System);
        assert_eq!(back.temperature, 0.3);
        assert_eq!(back.max_tokens, 512);
    }

    /// Ровно то тело, которое шлёт любой OpenAI-клиент, проигрывающий свою
    /// историю вызовов: у ассистента `"content": null`, весь смысл в
    /// `tool_calls`. Раньше это не совпадало ни с одним вариантом
    /// `MessageContent`, разбор тела падал целиком, и сервер отвечал
    /// «400 invalid request body» ещё до всякой обработки.
    #[test]
    fn an_assistant_message_with_null_content_is_accepted() {
        let raw = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "what is the weather"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{}"},
                    }],
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "52F"},
            ],
        });
        let parsed: ChatCompletionRequest =
            serde_json::from_value(raw).expect("a null assistant content must parse");
        let internal = to_internal_request(parsed, &defaults()).unwrap();
        assert_eq!(internal.messages.len(), 3);
        assert_eq!(internal.messages[1].role, Role::Assistant);
        assert_eq!(internal.messages[1].content, "");
    }

    /// Отсутствующее поле `content` — то же самое, что `null`.
    #[test]
    fn a_missing_content_field_is_accepted() {
        let raw = serde_json::json!({
            "model": "m",
            "messages": [{"role": "assistant"}],
        });
        let parsed: ChatCompletionRequest = serde_json::from_value(raw).unwrap();
        let internal = to_internal_request(parsed, &defaults()).unwrap();
        assert_eq!(internal.messages[0].content, "");
    }

    /// Входящее направление не трогаем: наш собственный сервер обязан
    /// принимать `role:"tool"` от клиентов, это законная часть протокола.
    /// Ломается только исходящее — там у ассистента нет `tool_calls`.
    #[test]
    fn an_inbound_tool_message_is_still_accepted() {
        let raw = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "q"},
                {"role": "tool", "tool_call_id": "call_1", "content": "42"},
            ],
        });
        let parsed: ChatCompletionRequest = serde_json::from_value(raw).unwrap();
        let internal = to_internal_request(parsed, &defaults()).unwrap();
        assert_eq!(internal.messages[1].role, Role::Tool);
        assert_eq!(internal.messages[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn openai_chunk_converts_to_internal() {
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-x".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "m".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some("tok".to_string()),
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
        };
        let internal = chunk_from_openai(chunk);
        assert_eq!(internal.content, "tok");
        assert_eq!(internal.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn model_list_shape() {
        let list = model_list(&["deepseek-chat", "deepseek-reasoner"], "pooprusteek", 1);
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["object"], "list");
        assert_eq!(json["data"][0]["id"], "deepseek-chat");
        assert_eq!(json["data"][1]["object"], "model");
    }

    #[test]
    fn error_envelope_shape() {
        let error = ErrorResponse::invalid_request("bad role");
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["message"], "bad role");
    }

    #[test]
    fn split_reasoning_extracts_leading_think_block() {
        let (reasoning, content) = split_reasoning("<think>ponder this</think>The answer is 4.");
        assert_eq!(reasoning.as_deref(), Some("ponder this"));
        assert_eq!(content, "The answer is 4.");

        // `<thinking>` variant + leading whitespace + trimmed content.
        let (reasoning, content) = split_reasoning("\n<thinking>  hmm  </thinking>\n\nDone");
        assert_eq!(reasoning.as_deref(), Some("hmm"));
        assert_eq!(content, "Done");
    }

    #[test]
    fn split_reasoning_leaves_plain_content_untouched() {
        let (reasoning, content) = split_reasoning("Just a normal answer with no reasoning.");
        assert!(reasoning.is_none());
        assert_eq!(content, "Just a normal answer with no reasoning.");

        // A mid-text mention of the tag is NOT reasoning (only a leading block).
        let (reasoning, content) = split_reasoning("Consider the <think> tag here.");
        assert!(reasoning.is_none());
        assert_eq!(content, "Consider the <think> tag here.");
    }

    #[test]
    fn split_reasoning_unterminated_is_all_reasoning() {
        // finish_reason == "length" mid-thought: budget spent on reasoning,
        // no visible answer — everything is reasoning, content empty.
        let (reasoning, content) = split_reasoning("<think>still working through it");
        assert_eq!(reasoning.as_deref(), Some("still working through it"));
        assert_eq!(content, "");
    }

    /// The streaming splitter, fed at ANY chunk boundary, must produce the
    /// same total (reasoning, content) as the pure `split_reasoning` — even
    /// when a `<think>`/`</think>` tag is torn across deltas.
    #[test]
    fn stream_splitter_matches_pure_split_at_every_boundary() {
        let cases = [
            "<think>reasoning here</think>final answer",
            "<thinking>multi\nline\nthought</thinking>the result",
            "no reasoning at all, just content",
            "  leading spaces then content",
            "<think>unterminated thought with no closer",
            "<think></think>empty reasoning block then answer",
        ];
        for whole in cases {
            let (want_reasoning, want_content) = split_reasoning(whole);
            let want_reasoning = want_reasoning.unwrap_or_default();
            // Feed one byte at a time — the worst case for split tags.
            let mut splitter = ReasoningStreamSplitter::new();
            let (mut got_reasoning, mut got_content) = (String::new(), String::new());
            let mut start = 0;
            while start < whole.len() {
                let mut end = start + 1;
                while !whole.is_char_boundary(end) {
                    end += 1;
                }
                let (r, c) = splitter.push(&whole[start..end]);
                got_reasoning.push_str(&r);
                got_content.push_str(&c);
                start = end;
            }
            let (r, c) = splitter.finish();
            got_reasoning.push_str(&r);
            got_content.push_str(&c);
            // The pure fn trims; the streaming one preserves interior text
            // verbatim, so compare on trimmed reasoning and exact content.
            assert_eq!(
                got_reasoning.trim(),
                want_reasoning,
                "reasoning for {whole:?}"
            );
            assert_eq!(got_content, want_content, "content for {whole:?}");
        }
    }

    #[test]
    fn stream_splitter_passes_plain_content_through() {
        let mut splitter = ReasoningStreamSplitter::new();
        let (r, c) = splitter.push("Hello, ");
        // First delta may hold a bit while ruling out an opener, but nothing
        // is ever misfiled as reasoning.
        assert!(r.is_empty());
        let (r2, c2) = splitter.push("world!");
        let (r3, c3) = splitter.finish();
        assert!(r2.is_empty() && r3.is_empty());
        assert_eq!(format!("{c}{c2}{c3}"), "Hello, world!");
    }
}
