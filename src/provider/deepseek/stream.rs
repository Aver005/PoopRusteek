//! Request-body construction for the DeepSeek completion endpoint, and the
//! SSE event parsing helpers that turn one `data: ...` line into an optional
//! text chunk plus an optional `parent_message_id` update.

use super::DeepseekProvider;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use crate::provider::prompt;
use crate::provider::{ChatMessage, CompletionRequest, Role};
use reqwest::{Response, header::HeaderValue};
use serde_json::{Value, json};

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
            .send_json_request("pow.challenge.request", CREATE_POW_URL, &headers, &body)
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
        let raw: super::pow::PowChallengeResponse =
            serde_json::from_str(&raw_text).map_err(|error| {
                debug_log::log(
                    "pow.challenge.parse",
                    format!("failed to parse challenge response json: {error}; raw={raw_text}"),
                );
                AppError::Json(error)
            })?;
        debug_log::log_json("pow.challenge.response", &raw);
        let challenge = raw.data.biz_data.challenge;
        // The solve is a synchronous CPU-bound wasm hash loop (tens to
        // hundreds of ms) — run it on the blocking pool so it can't stall
        // the async workers that drive the UI and other conversations.
        let solution = tokio::task::spawn_blocking(move || super::pow::solve_pow(&challenge))
            .await
            .map_err(|e| AppError::Provider(format!("PoW solver task failed: {e}")))?
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

    pub(super) async fn send_request(
        &self,
        request: &CompletionRequest,
    ) -> AppResult<(Response, String)> {
        let (system_prompt, non_system_messages) = prompt::split_system_prompt(&request.messages);
        let should_reset = {
            let state = self
                .session_state
                .lock()
                .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
            state.system_sent_for_session
                && prompt::is_first_conversational_send(&non_system_messages)
        };

        let session = self.ensure_session(should_reset).await?;
        let prompt = prompt::build_prompt(
            &non_system_messages,
            &system_prompt,
            session.system_sent_for_session,
        );
        // Counted from the prompt that is really sent — for a continuing
        // session that is only the tail, not the whole local history.
        let prompt_tokens = crate::context::budget_tokens(&prompt);
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
            return Err(Self::read_error_response(
                "completion.request",
                response,
                "Chat completion failed",
            )
            .await);
        }
        self.add_session_tokens(prompt_tokens);

        Ok((response, session.session_id))
    }

    pub(super) async fn fetch_remote_history(
        &self,
        session_id: &str,
    ) -> AppResult<Vec<ChatMessage>> {
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
                messages.push(ChatMessage {
                    role,
                    content,
                    name: None,
                    tool_call_id: None,
                    display_content: None,
                    tool_error: false,
                    ui_only: false,
                    created_at: String::new(),
                    total_tokens: None,
                    model: String::new(),
                    status: None,
                    think_elapsed_secs: 0.0,
                    references_count: 0,
                    search_triggered: false,
                });
            }
        }
        Ok(messages)
    }
}

pub(super) fn get_value_by_path<'a>(
    value: &'a Value,
    path: &[PathSegment<'_>],
) -> Option<&'a Value> {
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
        && let Some(event_v) = object.get("v")
    {
        let patch_path = object.get("p").and_then(Value::as_str);
        let operation = object.get("o").and_then(Value::as_str);

        if let Some(text) = event_v.as_str() {
            if text == "FINISHED" {
                return String::new();
            }
            if let Some(path) = patch_path
                && !path.contains("/content")
            {
                return String::new();
            }
            if let Some(op) = operation
                && op != "APPEND"
                && op != "SET"
            {
                return String::new();
            }
            return text.to_string();
        }

        if event_v.is_object() {
            let from_response =
                extract_response_fragments_content(event_v.get("response").unwrap_or(&Value::Null));
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

        let may_be_json = (trimmed.starts_with('{') && trimmed.ends_with('}'))
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
                && let Ok(parsed) = text.parse::<i64>()
            {
                return Some(parsed);
            }
        }
    }

    None
}

/// One recognized `data:` line from the completion SSE stream.
pub(super) struct StreamEvent {
    pub text: Option<String>,
    pub parent_message_id: Option<i64>,
    /// The server explicitly marked the response complete: `[DONE]` or a
    /// FINISHED status patch.
    pub finished: bool,
    /// The server reported a failure *inside* the stream rather than over
    /// HTTP status. Found by the harness: a rate-limited request answers 200
    /// OK and then sends
    /// `{"type":"error","content":"...","finish_reason":"rate_limit_reached"}`
    /// followed by a clean EOF. This frame used to match nothing, so it was
    /// logged as "skipped" and the turn ended as an empty *success* — an agent
    /// that silently did nothing while the provider had said exactly why.
    pub error: Option<StreamError>,
}

/// An in-stream error frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamError {
    /// The provider's own reason code, e.g. `rate_limit_reached`.
    pub finish_reason: Option<String>,
    /// Human-readable text, often localized (the rate-limit message is
    /// Chinese), so it is passed through rather than matched on.
    pub content: Option<String>,
}

impl StreamError {
    /// Message shown to the user and recorded in the trace. The provider's
    /// own text is kept verbatim (it is the ground truth, and localized), with
    /// an actionable line appended for the reason we can act on.
    pub fn message(&self) -> String {
        let detail = match (&self.finish_reason, &self.content) {
            (Some(reason), Some(content)) => format!("provider error ({reason}): {content}"),
            (Some(reason), None) => format!("provider error: {reason}"),
            (None, Some(content)) => format!("provider error: {content}"),
            (None, None) => "provider error (no detail given)".to_string(),
        };
        if self.is_rate_limit() {
            format!(
                "{detail} — too many requests; wait a moment, or raise `/rate` to throttle sends"
            )
        } else {
            detail
        }
    }

    /// Whether backing off is the right response. Retrying a rate limit
    /// immediately makes it worse.
    pub fn is_rate_limit(&self) -> bool {
        self.finish_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("rate_limit"))
    }
}

/// Recognize an in-stream error frame, whatever else the payload carries.
fn extract_stream_error(event: &Value) -> Option<StreamError> {
    let object = event.as_object()?;
    let is_error = object.get("type").and_then(Value::as_str) == Some("error")
        || object.get("finish_reason").and_then(Value::as_str) == Some("rate_limit_reached");
    if !is_error {
        return None;
    }
    Some(StreamError {
        finish_reason: object
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        content: object
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string),
    })
}

/// True when a single patch object sets the response status to FINISHED,
/// e.g. `{"p":"response/status","v":"FINISHED"}` or a bare `{"v":"FINISHED"}`.
fn patch_signals_finished(patch: &Value) -> bool {
    let Some(object) = patch.as_object() else {
        return false;
    };
    if object.get("v").and_then(Value::as_str) != Some("FINISHED") {
        return false;
    }
    match object.get("p").and_then(Value::as_str) {
        Some(path) => path.ends_with("status"),
        None => true,
    }
}

/// True when an event marks the end of the response: a status patch, a
/// terminal `{"o":"BATCH","v":[patches...]}` containing one, or a full
/// response object whose `status` is FINISHED.
fn event_signals_finished(event: &Value) -> bool {
    if patch_signals_finished(event) {
        return true;
    }
    if let Some(object) = event.as_object()
        && object.get("o").and_then(Value::as_str) == Some("BATCH")
        && let Some(patches) = object.get("v").and_then(Value::as_array)
        && patches.iter().any(patch_signals_finished)
    {
        return true;
    }
    get_value_by_path(
        event,
        &[
            PathSegment::Key("v"),
            PathSegment::Key("response"),
            PathSegment::Key("status"),
        ],
    )
    .and_then(Value::as_str)
        == Some("FINISHED")
}

pub(super) fn process_stream_line(line: &str) -> Option<StreamEvent> {
    let payload = crate::provider::sse::sse_data_payload(line)?;
    if payload.is_empty() {
        return None;
    }
    if payload == "[DONE]" {
        return Some(StreamEvent {
            text: None,
            parent_message_id: None,
            finished: true,
            error: None,
        });
    }

    let parsed = serde_json::from_str::<Value>(payload)
        .unwrap_or_else(|_| Value::String(payload.to_string()));
    let normalized = normalize_event_payload(parsed.clone());
    let parent_message_id = if normalized.is_object() {
        extract_parent_message_id(&normalized)
    } else {
        None
    };
    let finished = event_signals_finished(&normalized);

    let mut text_chunk = extract_text_from_event(&normalized);
    if text_chunk.is_empty() {
        if let Some(text) = normalized.as_str() {
            text_chunk = text.to_string();
        } else if let Some(text) = parsed.as_str() {
            text_chunk = text.to_string();
        }
    }

    // Checked before the "nothing recognized" bail-out below: an error frame
    // carries no text and no ids, so it would otherwise be dropped.
    let error = extract_stream_error(&normalized).or_else(|| extract_stream_error(&parsed));

    if text_chunk.is_empty() && parent_message_id.is_none() && !finished && error.is_none() {
        return None;
    }

    Some(StreamEvent {
        text: if text_chunk.is_empty() {
            None
        } else {
            Some(text_chunk)
        },
        parent_message_id,
        finished,
        error,
    })
}

#[cfg(test)]
mod tests {
    /// The frame that made the harness's "agent silently did nothing" run
    /// explicable: HTTP 200, an error inside the stream, then a clean EOF.
    #[test]
    fn recognizes_the_rate_limit_error_frame() {
        let line = r#"data: {"type":"error","content":"消息发送过于频繁，请稍后重试","clear_response":true,"finish_reason":"rate_limit_reached"}"#;
        let event = process_stream_line(line).expect("error frame must be recognized, not skipped");
        let error = event.error.expect("frame carries an error");
        assert!(error.is_rate_limit());
        assert_eq!(error.finish_reason.as_deref(), Some("rate_limit_reached"));
        let message = error.message();
        assert!(message.contains("rate_limit_reached"), "{message}");
        assert!(message.contains("too many requests"), "{message}");
        // No text, so nothing must leak into the assistant's reply.
        assert!(event.text.is_none());
    }

    #[test]
    fn a_type_error_frame_without_a_reason_is_still_an_error() {
        let line = r#"data: {"type":"error","content":"something broke"}"#;
        let event = process_stream_line(line).expect("recognized");
        let error = event.error.expect("carries an error");
        assert!(!error.is_rate_limit());
        assert_eq!(error.message(), "provider error: something broke");
    }

    #[test]
    fn ordinary_content_frames_carry_no_error() {
        let line = r#"data: {"p":"response/content","o":"APPEND","v":"hello"}"#;
        let event = process_stream_line(line).expect("recognized");
        assert!(event.error.is_none());
        assert_eq!(event.text.as_deref(), Some("hello"));
    }

    use super::*;

    #[test]
    fn done_marker_finishes_with_or_without_space() {
        for line in ["data: [DONE]", "data:[DONE]"] {
            let event = process_stream_line(line).expect("[DONE] must be recognized");
            assert!(event.finished);
            assert!(event.text.is_none());
        }
    }

    #[test]
    fn status_finished_patch_finishes_without_text() {
        let event = process_stream_line(r#"data: {"p":"response/status","v":"FINISHED"}"#)
            .expect("status patch must be recognized");
        assert!(event.finished);
        assert!(event.text.is_none());
    }

    #[test]
    fn bare_finished_value_finishes() {
        let event = process_stream_line(r#"data: {"v":"FINISHED"}"#)
            .expect("bare FINISHED must be recognized");
        assert!(event.finished);
        assert!(event.text.is_none());
    }

    #[test]
    fn terminal_batch_patch_finishes() {
        let line = r#"data: {"p":"response","o":"BATCH","v":[{"p":"status","v":"FINISHED"},{"p":"accumulated_token_usage","v":42}]}"#;
        let event = process_stream_line(line).expect("terminal batch must be recognized");
        assert!(event.finished);
    }

    #[test]
    fn content_append_is_text_not_finish() {
        let event = process_stream_line(
            r#"data: {"p":"response/fragments/-1/content","o":"APPEND","v":"hello"}"#,
        )
        .expect("content append must be recognized");
        assert_eq!(event.text.as_deref(), Some("hello"));
        assert!(!event.finished);
    }

    #[test]
    fn bare_v_chunk_is_text() {
        let event = process_stream_line(r#"data: {"v":" Length"}"#).unwrap();
        assert_eq!(event.text.as_deref(), Some(" Length"));
        assert!(!event.finished);
    }

    #[test]
    fn metadata_line_yields_parent_id_only() {
        let event = process_stream_line(
            r#"data: {"request_message_id":23,"response_message_id":24,"model_type":"default"}"#,
        )
        .unwrap();
        assert_eq!(event.parent_message_id, Some(24));
        assert!(event.text.is_none());
        assert!(!event.finished);
    }

    #[test]
    fn non_data_lines_are_ignored() {
        assert!(process_stream_line(": keep-alive").is_none());
        assert!(process_stream_line("event: message").is_none());
        assert!(process_stream_line("").is_none());
    }
}
