//! Request-body construction for the DeepSeek completion endpoint, and the
//! SSE event parsing helpers that turn one `data: ...` line into an optional
//! text chunk plus an optional `parent_message_id` update.

use super::DeepseekProvider;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use crate::provider::prompt;
use crate::provider::{ChatMessage, CompletionRequest, Role};
use reqwest::{header::HeaderValue, Response};
use serde_json::{json, Value};

const CREATE_POW_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const SESSION_HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history";
const TARGET_PATH: &str = "/api/v0/chat/completion";

pub(super) enum PathSegment<'a> {
    Key(&'a str),
    Index(usize),
}

impl DeepseekProvider {
    pub(super) async fn get_chat_headers(&self) -> AppResult<reqwest::header::HeaderMap> {
        let mut headers = self.auth_headers()?;
        let pow_b64 = self.solve_pow_challenge().await?;
        headers.insert(
            "x-ds-pow-response",
            HeaderValue::from_str(&pow_b64).map_err(|e| AppError::Provider(e.to_string()))?,
        );
        Ok(headers)
    }

    pub(super) async fn solve_pow_challenge(&self) -> AppResult<String> {
        let body = json!({ "target_path": TARGET_PATH });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request(
                "pow.challenge.request",
                CREATE_POW_URL,
                &headers,
                &body,
            )
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(Self::read_error_response(
                "pow.challenge.request",
                response,
                "PoW challenge HTTP",
            )
            .await);
        }

        let raw_text = response.text().await.map_err(|error| {
            debug_log::log(
                "pow.challenge.read_body",
                format!("failed to read challenge response body: {error}"),
            );
            AppError::Http(error)
        })?;
        debug_log::log(
            "pow.challenge.raw_body",
            format!("len={} body={}", raw_text.len(), raw_text),
        );
        let raw: super::pow::PowChallengeResponse = serde_json::from_str(&raw_text).map_err(|error| {
            debug_log::log(
                "pow.challenge.parse",
                format!("failed to parse challenge response json: {error}; raw={raw_text}"),
            );
            AppError::Json(error)
        })?;
        debug_log::log_json("pow.challenge.response", &raw);
        let challenge = raw.data.biz_data.challenge;
        let solution = super::pow::solve_pow(&challenge)
            .ok_or_else(|| AppError::Provider("Failed to solve PoW challenge".to_string()))?;
        debug_log::log_json("pow.challenge.solution", &solution);
        super::pow::encode_solution(&solution)
    }

    pub(super) fn build_body(
        &self,
        request: &CompletionRequest,
        prompt: String,
        session: &super::session::SessionSnapshot,
    ) -> Value {
        let model_type = prompt::resolve_model_type(&request.model, session.parent_message_id);
        let thinking_enabled = matches!(model_type, Some("expert"));

        json!({
            "prompt": prompt,
            "model": "deepseek-chat",
            "model_type": model_type,
            "stream": true,
            "temperature": request.temperature,
            "max_tokens": request.max_tokens,
            "ref_file_ids": [],
            "thinking_enabled": thinking_enabled,
            "search_enabled": false,
            "chat_session_id": session.session_id,
            "parent_message_id": session.parent_message_id,
        })
    }

    pub(super) async fn send_request(&self, request: &CompletionRequest) -> AppResult<(Response, String)> {
        let (system_prompt, non_system_messages) = prompt::split_system_prompt(&request.messages);
        let should_reset = {
            let state = self
                .session_state
                .lock()
                .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
            state.system_sent_for_session && non_system_messages.len() == 1
        };

        let session = self.ensure_session(should_reset).await?;
        let prompt = prompt::build_prompt(
            &non_system_messages,
            &system_prompt,
            session.system_sent_for_session,
        );
        let body = self.build_body(request, prompt, &session);
        debug_log::log_json(
            "completion.context",
            &json!({
                "request_model": request.model,
                "provider_model": self.model,
                "temperature": request.temperature,
                "max_tokens": request.max_tokens,
                "message_count": request.messages.len(),
                "session_id": session.session_id,
                "parent_message_id": session.parent_message_id,
                "system_sent_for_session": session.system_sent_for_session,
                "should_reset": should_reset,
                "prompt_preview": body["prompt"],
            }),
        );
        let headers = self.get_chat_headers().await?;

        let response = self
            .send_json_request("completion.request", COMPLETION_URL, &headers, &body)
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(
                Self::read_error_response("completion.request", response, "Chat completion failed")
                    .await,
            );
        }

        Ok((response, session.session_id))
    }

    pub(super) async fn fetch_remote_history(&self, session_id: &str) -> AppResult<Vec<ChatMessage>> {
        let body = json!({
            "session_id": session_id,
            "parent_message_id": Value::Null,
            "count": 1000,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request(
                "session.history.request",
                SESSION_HISTORY_URL,
                &headers,
                &body,
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response(
                "session.history.request",
                response,
                "Session history failed",
            )
            .await);
        }
        let payload: Value = response.json().await?;
        let items = payload["data"]["biz_data"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut messages = Vec::new();
        for item in &items {
            let role_str = item["role"].as_str().unwrap_or("user");
            let content = item["content"].as_str().unwrap_or("").to_string();
            let role = match role_str {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                _ => continue,
            };
            if role == Role::User || role == Role::Assistant {
                messages.push(ChatMessage { role, content, name: None, tool_call_id: None, display_content: None, tool_error: false, ui_only: false, created_at: String::new(), total_tokens: None, model: String::new(), status: None, think_elapsed_secs: 0.0, references_count: 0, search_triggered: false });
            }
        }
        Ok(messages)
    }
}

pub(super) fn get_value_by_path<'a>(value: &'a Value, path: &[PathSegment<'_>]) -> Option<&'a Value> {
    let mut current = value;
    for segment in path {
        current = match segment {
            PathSegment::Key(key) => current.get(*key)?,
            PathSegment::Index(index) => current.get(*index)?,
        };
    }
    Some(current)
}

fn to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Array(items) => items.iter().map(to_text).collect::<Vec<_>>().join(""),
        Value::Object(_) => {
            let from_text = value.get("text").map(to_text).unwrap_or_default();
            if !from_text.is_empty() {
                return from_text;
            }
            value.get("content").map(to_text).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn extract_response_fragments_content(response_node: &Value) -> String {
    let Some(fragments) = response_node.get("fragments").and_then(Value::as_array) else {
        return String::new();
    };

    fragments
        .iter()
        .filter_map(|fragment| fragment.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

pub(super) fn extract_text_from_event(event: &Value) -> String {
    if let Some(object) = event.as_object()
        && let Some(event_v) = object.get("v") {
            let patch_path = object.get("p").and_then(Value::as_str);
            let operation = object.get("o").and_then(Value::as_str);

            if let Some(text) = event_v.as_str() {
                if text == "FINISHED" {
                    return String::new();
                }
                if let Some(path) = patch_path
                    && !path.contains("/content") {
                        return String::new();
                    }
                if let Some(op) = operation
                    && op != "APPEND" && op != "SET" {
                        return String::new();
                    }
                return text.to_string();
            }

            if event_v.is_object() {
                let from_response = extract_response_fragments_content(
                    event_v.get("response").unwrap_or(&Value::Null),
                );
                if !from_response.is_empty() {
                    return from_response;
                }
            }
        }

    let paths = [
        [
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("delta"),
            PathSegment::Key("content"),
        ]
        .as_slice(),
        [
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("delta"),
            PathSegment::Key("reasoning_content"),
        ]
        .as_slice(),
        [
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("message"),
            PathSegment::Key("content"),
        ]
        .as_slice(),
        [
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("text"),
        ]
        .as_slice(),
        [
            PathSegment::Key("data"),
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("delta"),
            PathSegment::Key("content"),
        ]
        .as_slice(),
        [
            PathSegment::Key("data"),
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("message"),
            PathSegment::Key("content"),
        ]
        .as_slice(),
        [
            PathSegment::Key("data"),
            PathSegment::Key("message"),
            PathSegment::Key("content"),
        ]
        .as_slice(),
        [PathSegment::Key("delta"), PathSegment::Key("content")].as_slice(),
        [PathSegment::Key("message"), PathSegment::Key("content")].as_slice(),
    ];

    for path in paths {
        if let Some(value) = get_value_by_path(event, path) {
            let text = to_text(value);
            if !text.is_empty() {
                return text;
            }
        }
    }

    String::new()
}

pub(super) fn normalize_event_payload(payload: Value) -> Value {
    let mut current = payload;

    for _ in 0..3 {
        let Some(text) = current.as_str() else {
            return current;
        };

        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Value::String(String::new());
        }

        let may_be_json =
            (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
                || (trimmed.starts_with('"') && trimmed.ends_with('"'));
        if !may_be_json {
            return Value::String(trimmed.to_string());
        }

        match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) => current = parsed,
            Err(_) => return Value::String(trimmed.to_string()),
        }
    }

    current
}

pub(super) fn extract_parent_message_id(event: &Value) -> Option<i64> {
    let paths = [
        [PathSegment::Key("response_message_id")].as_slice(),
        [PathSegment::Key("parent_message_id")].as_slice(),
        [PathSegment::Key("message_id")].as_slice(),
        [PathSegment::Key("id")].as_slice(),
        [
            PathSegment::Key("v"),
            PathSegment::Key("response"),
            PathSegment::Key("message_id"),
        ]
        .as_slice(),
        [
            PathSegment::Key("v"),
            PathSegment::Key("response"),
            PathSegment::Key("parent_id"),
        ]
        .as_slice(),
        [
            PathSegment::Key("data"),
            PathSegment::Key("parent_message_id"),
        ]
        .as_slice(),
        [PathSegment::Key("data"), PathSegment::Key("message_id")].as_slice(),
        [
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("message"),
            PathSegment::Key("id"),
        ]
        .as_slice(),
        [
            PathSegment::Key("choices"),
            PathSegment::Index(0),
            PathSegment::Key("message_id"),
        ]
        .as_slice(),
    ];

    for path in paths {
        if let Some(value) = get_value_by_path(event, path) {
            if let Some(number) = value.as_i64() {
                return Some(number);
            }
            if let Some(text) = value.as_str()
                && let Ok(parsed) = text.parse::<i64>() {
                    return Some(parsed);
                }
        }
    }

    None
}

pub(super) fn process_stream_line(line: &str) -> Option<(Option<String>, Option<i64>)> {
    let trimmed = line.trim();
    if !trimmed.starts_with("data:") {
        return None;
    }

    let payload = trimmed[5..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }

    let parsed = serde_json::from_str::<Value>(payload)
        .unwrap_or_else(|_| Value::String(payload.to_string()));
    let normalized = normalize_event_payload(parsed.clone());
    let parent_message_id = if normalized.is_object() {
        extract_parent_message_id(&normalized)
    } else {
        None
    };

    let mut text_chunk = extract_text_from_event(&normalized);
    if text_chunk.is_empty() {
        if let Some(text) = normalized.as_str() {
            text_chunk = text.to_string();
        } else if let Some(text) = parsed.as_str() {
            text_chunk = text.to_string();
        }
    }

    if text_chunk.is_empty() && parent_message_id.is_none() {
        return None;
    }

    Some((
        if text_chunk.is_empty() {
            None
        } else {
            Some(text_chunk)
        },
        parent_message_id,
    ))
}
