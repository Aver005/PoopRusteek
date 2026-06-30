use super::pow;
use super::types;
use super::*;
use crate::config::ProviderConfig;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Response,
};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::time::sleep;

const DEEPSEEK_HOST: &str = "chat.deepseek.com";
const API_BASE: &str = "https://chat.deepseek.com/api/v0";

// Core chat
const CREATE_POW_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
const SESSION_HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history";
const HISTORY_MESSAGES_URL: &str = "https://chat.deepseek.com/api/v0/chat/history_messages";
const TARGET_PATH: &str = "/api/v0/chat/completion";

// Session management
const FETCH_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
const DELETE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete";
const DELETE_ALL_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete_all";
const UPDATE_TITLE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_title";
const UPDATE_PINNED_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_pinned";

// Message actions
const MESSAGE_FEEDBACK_URL: &str = "https://chat.deepseek.com/api/v0/chat/message_feedback";
const EDIT_MESSAGE_URL: &str = "https://chat.deepseek.com/api/v0/chat/edit_message";
const REGENERATE_URL: &str = "https://chat.deepseek.com/api/v0/chat/regenerate";
const CONTINUE_URL: &str = "https://chat.deepseek.com/api/v0/chat/continue";
const STOP_STREAM_URL: &str = "https://chat.deepseek.com/api/v0/chat/stop_stream";
const RESUME_STREAM_URL: &str = "https://chat.deepseek.com/api/v0/chat/resume_stream";

// File
const UPLOAD_FILE_URL: &str = "https://chat.deepseek.com/api/v0/file/upload_file";
const FETCH_FILES_URL: &str = "https://chat.deepseek.com/api/v0/file/fetch_files";
const FORK_FILE_TASK_URL: &str = "https://chat.deepseek.com/api/v0/file/fork_file_task";

// Share
const CREATE_SHARE_URL: &str = "https://chat.deepseek.com/api/v0/share/create";
const LIST_SHARES_URL: &str = "https://chat.deepseek.com/api/v0/share/list";
const SHARE_CONTENT_URL: &str = "https://chat.deepseek.com/api/v0/share/content";
const DELETE_SHARE_URL: &str = "https://chat.deepseek.com/api/v0/share/delete";
const FORK_SHARE_URL: &str = "https://chat.deepseek.com/api/v0/share/fork";

// Search
const INDEX_PREPARE_URL: &str = "https://chat.deepseek.com/api/v0/index/prepare";
const INDEX_QUERY_URL: &str = "https://chat.deepseek.com/api/v0/index/query";

// User
const CURRENT_USER_URL: &str = "https://chat.deepseek.com/api/v0/users/current";
const USER_SETTINGS_URL: &str = "https://chat.deepseek.com/api/v0/users/settings";
const UPDATE_USER_SETTINGS_URL: &str = "https://chat.deepseek.com/api/v0/users/update_settings";
const LOGOUT_ALL_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/users/logout_all_sessions";
const SET_BIRTHDAY_URL: &str = "https://chat.deepseek.com/api/v0/users/set_birthday";

// Client settings & telemetry
const CLIENT_SETTINGS_URL: &str = "https://chat.deepseek.com/api/v0/client/settings";
const CLIENT_SETTINGS_REPORT_URL: &str = "https://chat.deepseek.com/api/v0/client/settings/report";
const CLIENT_SPAN_URL: &str = "https://chat.deepseek.com/api/v0/client/span";

// Export
const DOWNLOAD_EXPORT_HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/download_export_history";
const EXPORT_ALL_URL: &str = "https://chat.deepseek.com/api/v0/export_all";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 YaBrowser/26.3.0.0 Safari/537.36";


#[derive(Debug, Default)]
struct SessionState {
    session_id: Option<String>,
    parent_message_id: Option<i64>,
    system_sent_for_session: bool,
}

#[derive(Debug, Clone)]
struct SessionSnapshot {
    session_id: String,
    parent_message_id: Option<i64>,
    system_sent_for_session: bool,
}

enum PathSegment<'a> {
    Key(&'a str),
    Index(usize),
}

pub struct DeepseekProvider {
    client: Client,
    token: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    session_state: Mutex<SessionState>,
    rate_limit_ms: u64,
    max_retries: i32,
    last_request: Mutex<Instant>,
}

impl DeepseekProvider {
    pub fn new(config: &ProviderConfig, rate_limit_ms: u64, max_retries: i32) -> AppResult<Self> {
        let client = Client::builder().build()?;

        let provider = Self {
            client,
            token: config.token.clone(),
            model: config.model.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            session_state: Mutex::new(SessionState::default()),
            rate_limit_ms,
            max_retries,
            last_request: Mutex::new(Instant::now()),
        };

        debug_log::log(
            "provider.new",
            format!(
                "DeepSeek provider created; model={}, temperature={}, max_tokens={}, token_present={}",
                provider.model,
                provider.temperature,
                provider.max_tokens,
                !provider.token.is_empty()
            ),
        );

        Ok(provider)
    }

    fn auth_headers(&self) -> AppResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("Host", HeaderValue::from_static(DEEPSEEK_HOST));
        headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        headers.insert("Accept-Encoding", HeaderValue::from_static("gzip"));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("x-client-platform", HeaderValue::from_static("android"));
        headers.insert("x-client-version", HeaderValue::from_static("1.8.0"));
        headers.insert("x-client-locale", HeaderValue::from_static("zh_CN"));
        headers.insert("accept-charset", HeaderValue::from_static("UTF-8"));
        let bearer = format!("Bearer {}", self.token);
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&bearer)
                .map_err(|e| AppError::Provider(format!("Invalid auth header: {e}")))?,
        );
        Ok(headers)
    }

    fn redact_value(key: &str, value: &str) -> String {
        let lower = key.to_ascii_lowercase();
        if lower == "authorization" {
            if value.len() > 24 {
                let head = crate::util::truncate_at_char_boundary(value, 16);
                let tail_start = value.len().saturating_sub(8);
                let tail_start = if value.is_char_boundary(tail_start) {
                    tail_start
                } else {
                    let mut i = tail_start;
                    while i < value.len() && !value.is_char_boundary(i) {
                        i += 1;
                    }
                    i
                };
                return format!("{}...{}", head, &value[tail_start..]);
            }
            return "<redacted>".to_string();
        }
        if lower == "x-ds-pow-response" {
            return format!("<base64:{} chars>", value.len());
        }
        value.to_string()
    }

    fn headers_to_debug_json(headers: &HeaderMap) -> Value {
        let mut map = serde_json::Map::new();
        for (key, value) in headers {
            let raw = value
                .to_str()
                .map(|text| Self::redact_value(key.as_str(), text))
                .unwrap_or_else(|_| "<binary>".to_string());
            map.insert(key.to_string(), Value::String(raw));
        }
        Value::Object(map)
    }

    fn log_http_request(&self, action: &str, url: &str, headers: &HeaderMap, body: &Value) {
        debug_log::log_json(
            action,
            &json!({
                "url": url,
                "headers": Self::headers_to_debug_json(headers),
                "body": body,
            }),
        );
    }

    async fn enforce_rate_limit(&self) {
        let elapsed = self
            .last_request
            .lock()
            .map(|last| last.elapsed())
            .unwrap_or(Duration::from_secs(60));
        let min_interval = Duration::from_millis(self.rate_limit_ms);
        if elapsed < min_interval {
            sleep(min_interval - elapsed).await;
        }
        let _ = self.last_request.lock().map(|mut last| *last = Instant::now());
    }

    async fn send_json_request(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
    ) -> AppResult<Response> {
        self.enforce_rate_limit().await;

        let max_attempts = match self.max_retries {
            -1 => usize::MAX,
            0 => 1,
            n => (n as usize) + 1,
        };

        let mut attempt = 0;
        loop {
            self.log_http_request(action, url, headers, body);

            match self.client.post(url).headers(headers.clone()).json(body).send().await {
                Ok(response) => {
                    let status = response.status();
                    debug_log::log(
                        action,
                        format!("response status={status} headers={}", Self::headers_to_debug_json(response.headers())),
                    );

                    if !status.is_server_error() || attempt + 1 >= max_attempts {
                        return Ok(response);
                    }

                    attempt += 1;
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32 - 1));
                    let capped = delay.min(Duration::from_secs(30));
                    tracing::warn!("{action} server error {status}, retry {attempt}/{max_attempts} in {capped:?}");
                    sleep(capped).await;
                }
                Err(error) => {
                    if attempt + 1 >= max_attempts {
                        debug_log::log(
                            action,
                            format!("request failed before HTTP response: {error}"),
                        );
                        return Err(AppError::Http(error));
                    }
                    attempt += 1;
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32 - 1));
                    let capped = delay.min(Duration::from_secs(30));
                    tracing::warn!("{action} connection error: {error}, retry {attempt}/{max_attempts} in {capped:?}");
                    sleep(capped).await;
                }
            }
        }
    }

    async fn read_error_response(action: &str, response: Response, label: &str) -> AppError {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        debug_log::log(
            action,
            format!("response error status={status} body={text}"),
        );
        AppError::Provider(format!("{label}: {status} {text}"))
    }

    async fn get_chat_headers(&self) -> AppResult<HeaderMap> {
        let mut headers = self.auth_headers()?;
        let pow_b64 = self.solve_pow_challenge().await?;
        headers.insert(
            "x-ds-pow-response",
            HeaderValue::from_str(&pow_b64).map_err(|e| AppError::Provider(e.to_string()))?,
        );
        Ok(headers)
    }

    async fn solve_pow_challenge(&self) -> AppResult<String> {
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
        let raw: pow::PowChallengeResponse = serde_json::from_str(&raw_text).map_err(|error| {
            debug_log::log(
                "pow.challenge.parse",
                format!("failed to parse challenge response json: {error}; raw={raw_text}"),
            );
            AppError::Json(error)
        })?;
        debug_log::log_json("pow.challenge.response", &raw);
        let challenge = raw.data.biz_data.challenge;
        let solution = pow::solve_pow(&challenge)
            .ok_or_else(|| AppError::Provider("Failed to solve PoW challenge".to_string()))?;
        debug_log::log_json("pow.challenge.solution", &solution);
        pow::encode_solution(&solution)
    }

    async fn create_session(&self) -> AppResult<String> {
        let body = json!({ "character_id": Value::Null });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request(
                "session.create.request",
                CREATE_SESSION_URL,
                &headers,
                &body,
            )
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(Self::read_error_response(
                "session.create.request",
                response,
                "Session creation failed",
            )
            .await);
        }

        let payload: Value = response.json().await.map_err(|error| {
            debug_log::log(
                "session.create.parse",
                format!("failed to parse session response json: {error}"),
            );
            AppError::Http(error)
        })?;
        debug_log::log_json("session.create.response", &payload);
        let session_id = payload["data"]["biz_data"]["chat_session"]["id"]
            .as_str()
            .map(|id| id.to_string())
            .ok_or_else(|| AppError::Provider("Invalid session payload: missing chat_session.id".to_string()))?;
        debug_log::log("session.create.success", format!("session_id={session_id}"));
        Ok(session_id)
    }

    async fn ensure_session(&self, should_reset: bool) -> AppResult<SessionSnapshot> {
        {
            let state = self
                .session_state
                .lock()
                .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;

            if !should_reset {
                if let Some(session_id) = &state.session_id {
                    return Ok(SessionSnapshot {
                        session_id: session_id.clone(),
                        parent_message_id: state.parent_message_id,
                        system_sent_for_session: state.system_sent_for_session,
                    });
                }
            }
        }

        let session_id = self.create_session().await?;
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
        state.session_id = Some(session_id.clone());
        state.parent_message_id = None;
        state.system_sent_for_session = false;
        debug_log::log(
            "session.ensure",
            format!("initialized fresh session_id={session_id}, reset={should_reset}"),
        );

        Ok(SessionSnapshot {
            session_id,
            parent_message_id: None,
            system_sent_for_session: false,
        })
    }

    fn build_body(
        &self,
        request: &CompletionRequest,
        prompt: String,
        session: &SessionSnapshot,
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

    async fn send_request(&self, request: &CompletionRequest) -> AppResult<(Response, String)> {
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

    fn get_value_by_path<'a>(value: &'a Value, path: &[PathSegment<'_>]) -> Option<&'a Value> {
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
            Value::Array(items) => items.iter().map(Self::to_text).collect::<Vec<_>>().join(""),
            Value::Object(_) => {
                let from_text = value.get("text").map(Self::to_text).unwrap_or_default();
                if !from_text.is_empty() {
                    return from_text;
                }
                value.get("content").map(Self::to_text).unwrap_or_default()
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

    fn extract_text_from_event(event: &Value) -> String {
        if let Some(object) = event.as_object() {
            if let Some(event_v) = object.get("v") {
                let patch_path = object.get("p").and_then(Value::as_str);
                let operation = object.get("o").and_then(Value::as_str);

                if let Some(text) = event_v.as_str() {
                    if text == "FINISHED" {
                        return String::new();
                    }
                    if let Some(path) = patch_path {
                        if !path.contains("/content") {
                            return String::new();
                        }
                    }
                    if let Some(op) = operation {
                        if op != "APPEND" && op != "SET" {
                            return String::new();
                        }
                    }
                    return text.to_string();
                }

                if event_v.is_object() {
                    let from_response = Self::extract_response_fragments_content(
                        event_v.get("response").unwrap_or(&Value::Null),
                    );
                    if !from_response.is_empty() {
                        return from_response;
                    }
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
            if let Some(value) = Self::get_value_by_path(event, path) {
                let text = Self::to_text(value);
                if !text.is_empty() {
                    return text;
                }
            }
        }

        String::new()
    }

    fn normalize_event_payload(payload: Value) -> Value {
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

    fn extract_parent_message_id(event: &Value) -> Option<i64> {
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
            if let Some(value) = Self::get_value_by_path(event, path) {
                if let Some(number) = value.as_i64() {
                    return Some(number);
                }
                if let Some(text) = value.as_str() {
                    if let Ok(parsed) = text.parse::<i64>() {
                        return Some(parsed);
                    }
                }
            }
        }

        None
    }

    fn process_stream_line(line: &str) -> Option<(Option<String>, Option<i64>)> {
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
        let normalized = Self::normalize_event_payload(parsed.clone());
        let parent_message_id = if normalized.is_object() {
            Self::extract_parent_message_id(&normalized)
        } else {
            None
        };

        let mut text_chunk = Self::extract_text_from_event(&normalized);
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

    /// Enhanced SSE parser that handles all DeepSeek SSE event types.
    /// Processes raw SSE lines with `event:` and `data:` headers,
    /// returning a structured event type.
    fn parse_sse_event(
        event_name: Option<&str>,
        data: &str,
    ) -> Option<types::ParsedSSEEvent> {
        let data = data.trim();
        if data.is_empty() {
            return None;
        }

        let parsed: Value = serde_json::from_str(data).ok()?;

        match event_name {
            Some("ready") => {
                let ev: types::CompletionReadyEvent =
                    serde_json::from_value(parsed).ok()?;
                Some(types::ParsedSSEEvent::Ready(ev))
            }
            Some("update_session") => {
                let ev: types::CompletionUpdateSessionEvent =
                    serde_json::from_value(parsed).ok()?;
                Some(types::ParsedSSEEvent::UpdateSession(ev))
            }
            Some("title") => {
                let ev: types::CompletionTitleEvent =
                    serde_json::from_value(parsed).ok()?;
                Some(types::ParsedSSEEvent::Title(ev))
            }
            Some("close") => {
                let ev: types::CompletionCloseEvent =
                    serde_json::from_value(parsed).ok()?;
                Some(types::ParsedSSEEvent::Close(ev))
            }
            _ => {
                let normalized = Self::normalize_event_payload(parsed.clone());

                if normalized.is_object() {
                    let obj = normalized.as_object()?;

                    let op = obj.get("o").and_then(Value::as_str);
                    let path = obj.get("p").and_then(Value::as_str);

                    match (op, path) {
                        (Some("APPEND"), Some(_)) if obj.get("v").and_then(Value::as_array).is_some() => {
                            let ev: types::CompletionFragmentAppendEvent =
                                serde_json::from_value(normalized).ok()?;
                            Some(types::ParsedSSEEvent::FragmentAppend(ev))
                        }
                        (Some("APPEND"), Some(_)) => {
                            let ev: types::CompletionContentAppendEvent =
                                serde_json::from_value(normalized).ok()?;
                            Some(types::ParsedSSEEvent::ContentAppend(ev))
                        }
                        (Some("SET"), Some(_)) => {
                            let ev: types::CompletionFieldSetEvent =
                                serde_json::from_value(normalized).ok()?;
                            Some(types::ParsedSSEEvent::FieldSet(ev))
                        }
                        (Some("BATCH"), Some(_)) => {
                            let ev: types::CompletionBatchEvent =
                                serde_json::from_value(normalized).ok()?;
                            Some(types::ParsedSSEEvent::Batch(ev))
                        }
                        _ => {
                            if obj.contains_key("v") && !obj.contains_key("p") && !obj.contains_key("o") {
                                if obj.get("v").and_then(Value::as_object).is_some() {
                                    if obj["v"].get("response").is_some() {
                                        let ev: types::CompletionResponseEvent =
                                            serde_json::from_value(normalized).ok()?;
                                        Some(types::ParsedSSEEvent::Response(ev))
                                    } else {
                                        Some(types::ParsedSSEEvent::Unknown(
                                            serde_json::to_string(&normalized).unwrap_or_default(),
                                        ))
                                    }
                                } else if let Some(text) = obj.get("v").and_then(Value::as_str) {
                                    Some(types::ParsedSSEEvent::TokenDelta(
                                        types::CompletionTokenDeltaEvent {
                                            v: text.to_string(),
                                        },
                                    ))
                                } else {
                                    Some(types::ParsedSSEEvent::Unknown(
                                        serde_json::to_string(&normalized).unwrap_or_default(),
                                    ))
                                }
                            } else {
                                Some(types::ParsedSSEEvent::Unknown(
                                    serde_json::to_string(&normalized).unwrap_or_default(),
                                ))
                            }
                        }
                    }
                } else {
                    Some(types::ParsedSSEEvent::Unknown(data.to_string()))
                }
            }
        }
    }

    fn mark_session_after_success(
        &self,
        session_id: &str,
        parent_message_id: Option<i64>,
    ) -> AppResult<()> {
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;

        if state.session_id.as_deref() == Some(session_id) {
            state.system_sent_for_session = true;
            if parent_message_id.is_some() {
                state.parent_message_id = parent_message_id;
            }
        }

        Ok(())
    }

    async fn fetch_remote_history(&self, session_id: &str) -> AppResult<Vec<ChatMessage>> {
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
                messages.push(ChatMessage { role, content, name: None, tool_call_id: None, display_content: None, tool_error: false, created_at: String::new(), total_tokens: None, model: String::new(), status: None, think_elapsed_secs: 0.0, references_count: 0, search_triggered: false });
            }
        }
        Ok(messages)
    }

    // ─── Generic GET request helper ────────────────────────────

    async fn send_get_request(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
    ) -> AppResult<Response> {
        self.enforce_rate_limit().await;

        let max_attempts = match self.max_retries {
            -1 => usize::MAX,
            0 => 1,
            n => (n as usize) + 1,
        };

        let mut attempt = 0;
        loop {
            self.log_http_request(action, url, headers, &Value::Null);

            match self.client.get(url).headers(headers.clone()).send().await {
                Ok(response) => {
                    let status = response.status();
                    debug_log::log(
                        action,
                        format!("response status={status}"),
                    );

                    if !status.is_server_error() || attempt + 1 >= max_attempts {
                        return Ok(response);
                    }

                    attempt += 1;
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32 - 1));
                    let capped = delay.min(Duration::from_secs(30));
                    tracing::warn!("{action} server error {status}, retry {attempt}/{max_attempts} in {capped:?}");
                    sleep(capped).await;
                }
                Err(error) => {
                    if attempt + 1 >= max_attempts {
                        return Err(AppError::Http(error));
                    }
                    attempt += 1;
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt as u32 - 1));
                    let capped = delay.min(Duration::from_secs(30));
                    tracing::warn!("{action} connection error: {error}, retry {attempt}/{max_attempts} in {capped:?}");
                    sleep(capped).await;
                }
            }
        }
    }

    // ─── Session Management ────────────────────────────────────

    /// Fetch paginated list of remote sessions.
    pub async fn fetch_remote_sessions(
        &self,
        pinned: Option<bool>,
    ) -> AppResult<types::PaginatedResponse<types::ChatSession>> {
        let mut url = FETCH_SESSIONS_URL.to_string();
        if let Some(p) = pinned {
            url.push_str(&format!("?lte_cursor.pinned={p}"));
        }

        let headers = self.auth_headers()?;
        let response = self.send_get_request("sessions.fetch", &url, &headers).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Self::read_error_response("sessions.fetch", response, "Fetch sessions failed").await);
        }

        let payload: types::ApiResponse<types::PaginatedResponse<types::ChatSession>> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Delete a remote session.
    pub async fn delete_remote_session(&self, session_id: &str) -> AppResult<()> {
        let body = json!({ "chat_session_id": session_id });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("session.delete", DELETE_SESSION_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("session.delete", response, "Delete session failed").await);
        }
        Ok(())
    }

    /// Delete all remote sessions.
    pub async fn delete_all_remote_sessions(&self) -> AppResult<()> {
        let body = json!({});
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("session.delete_all", DELETE_ALL_SESSIONS_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("session.delete_all", response, "Delete all sessions failed").await);
        }
        Ok(())
    }

    /// Rename a remote session.
    pub async fn rename_remote_session(&self, session_id: &str, title: &str) -> AppResult<()> {
        let body = json!({ "chat_session_id": session_id, "title": title });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("session.rename", UPDATE_TITLE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("session.rename", response, "Rename session failed").await);
        }
        Ok(())
    }

    /// Pin or unpin a remote session.
    pub async fn pin_remote_session(&self, session_id: &str, pinned: bool) -> AppResult<()> {
        let body = json!({ "chat_session_id": session_id, "pinned": pinned });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("session.pin", UPDATE_PINNED_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("session.pin", response, "Pin session failed").await);
        }
        Ok(())
    }

    // ─── Message Actions ───────────────────────────────────────

    /// Send like/dislike feedback for a message.
    pub async fn send_message_feedback(
        &self,
        session_id: &str,
        message_id: i64,
        feedback: &str,
    ) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "message_id": message_id,
            "feedback": feedback,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("message.feedback", MESSAGE_FEEDBACK_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("message.feedback", response, "Feedback failed").await);
        }
        Ok(())
    }

    /// Edit a message (requires PoW).
    pub async fn edit_message(
        &self,
        session_id: &str,
        message_id: i64,
        prompt: &str,
        thinking_enabled: bool,
        search_enabled: bool,
    ) -> AppResult<()> {
        let pow_b64 = self.solve_pow_challenge().await?;
        let mut headers = self.auth_headers()?;
        headers.insert(
            "x-ds-pow-response",
            HeaderValue::from_str(&pow_b64).map_err(|e| AppError::Provider(e.to_string()))?,
        );
        let body = json!({
            "chat_session_id": session_id,
            "message_id": message_id,
            "prompt": prompt,
            "ref_file_ids": [],
            "thinking_enabled": thinking_enabled,
            "search_enabled": search_enabled,
        });
        let response = self
            .send_json_request("message.edit", EDIT_MESSAGE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("message.edit", response, "Edit message failed").await);
        }
        Ok(())
    }

    /// Regenerate the last assistant response (requires PoW).
    pub async fn regenerate_message(
        &self,
        session_id: &str,
        parent_message_id: i64,
        thinking_enabled: bool,
        search_enabled: bool,
    ) -> AppResult<()> {
        let pow_b64 = self.solve_pow_challenge().await?;
        let mut headers = self.auth_headers()?;
        headers.insert(
            "x-ds-pow-response",
            HeaderValue::from_str(&pow_b64).map_err(|e| AppError::Provider(e.to_string()))?,
        );
        let body = json!({
            "chat_session_id": session_id,
            "parent_message_id": parent_message_id,
            "model_type": null,
            "thinking_enabled": thinking_enabled,
            "search_enabled": search_enabled,
            "ref_file_ids": [],
        });
        let response = self
            .send_json_request("message.regenerate", REGENERATE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("message.regenerate", response, "Regenerate failed").await);
        }
        Ok(())
    }

    /// Continue an incomplete assistant response.
    pub async fn continue_message(
        &self,
        session_id: &str,
        response_message_id: i64,
    ) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "response_message_id": response_message_id,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("message.continue", CONTINUE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("message.continue", response, "Continue failed").await);
        }
        Ok(())
    }

    /// Stop an active stream.
    pub async fn stop_stream(
        &self,
        session_id: &str,
        response_message_id: i64,
    ) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "response_message_id": response_message_id,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("message.stop_stream", STOP_STREAM_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("message.stop_stream", response, "Stop stream failed").await);
        }
        Ok(())
    }

    /// Resume a stopped stream.
    pub async fn resume_stream(
        &self,
        session_id: &str,
        response_message_id: i64,
    ) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "response_message_id": response_message_id,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("message.resume_stream", RESUME_STREAM_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("message.resume_stream", response, "Resume stream failed").await);
        }
        Ok(())
    }

    // ─── File Operations ───────────────────────────────────────

    /// Upload a file. Returns the uploaded file info.
    pub async fn upload_file(&self, file_path: &str) -> AppResult<types::UploadedFile> {
        use reqwest::multipart;

        let pow_b64 = self.solve_pow_challenge().await?;
        let mut headers = self.auth_headers()?;
        headers.insert(
            "x-ds-pow-response",
            HeaderValue::from_str(&pow_b64).map_err(|e| AppError::Provider(e.to_string()))?,
        );
        headers.insert(
            "x-thinking-enabled",
            HeaderValue::from_static("false"),
        );
        headers.insert(
            "x-model-type",
            HeaderValue::from_static("default"),
        );

        let path = std::path::Path::new(file_path);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let file_size = std::fs::metadata(file_path)
            .map(|m| m.len())
            .unwrap_or(0)
            .to_string();

        headers.insert(
            "x-file-size",
            HeaderValue::from_str(&file_size).map_err(|e| AppError::Provider(e.to_string()))?,
        );

        let file_bytes = tokio::fs::read(file_path).await.map_err(AppError::Io)?;
        let file_part = multipart::Part::bytes(file_bytes)
            .file_name(file_name.clone())
            .mime_str("application/octet-stream")
            .map_err(|e| AppError::Custom(e.to_string()))?;

        let form = multipart::Form::new().part("file", file_part);

        self.enforce_rate_limit().await;
        let response = self
            .client
            .post(UPLOAD_FILE_URL)
            .headers(headers)
            .multipart(form)
            .send()
            .await
            .map_err(AppError::Http)?;

        let status = response.status();
        if !status.is_success() {
            return Err(Self::read_error_response("file.upload", response, "File upload failed").await);
        }

        let payload: types::ApiResponse<types::UploadedFile> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Fetch uploaded files by their IDs.
    pub async fn fetch_uploaded_files(
        &self,
        file_ids: &[String],
    ) -> AppResult<Vec<types::FetchedFile>> {
        let query: Vec<String> = file_ids
            .iter()
            .map(|id| format!("file_ids={}", urlencoding(id)))
            .collect();
        let url = if query.is_empty() {
            FETCH_FILES_URL.to_string()
        } else {
            format!("{}?{}", FETCH_FILES_URL, query.join("&"))
        };

        let headers = self.auth_headers()?;
        let response = self.send_get_request("file.fetch", &url, &headers).await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("file.fetch", response, "Fetch files failed").await);
        }

        let payload: types::ApiResponse<types::FetchFilesData> =
            response.json().await?;
        Ok(payload.data.biz_data.files)
    }

    /// Fork a file task (re-process a file).
    pub async fn fork_file_task(&self, file_id: &str) -> AppResult<types::FetchedFile> {
        let body = json!({ "file_id": file_id });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("file.fork", FORK_FILE_TASK_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("file.fork", response, "Fork file task failed").await);
        }
        let payload: types::ApiResponse<types::FetchedFile> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    // ─── Share Operations ──────────────────────────────────────

    /// Create a share link for messages in a session.
    pub async fn create_share(
        &self,
        session_id: &str,
        message_ids: &[i64],
    ) -> AppResult<types::CreateShareData> {
        let body = json!({
            "chat_session_id": session_id,
            "message_ids": message_ids,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("share.create", CREATE_SHARE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("share.create", response, "Create share failed").await);
        }
        let payload: types::ApiResponse<types::CreateShareData> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// List shares.
    pub async fn list_shares(&self, count: i64) -> AppResult<types::ShareListData> {
        let url = format!("{LIST_SHARES_URL}?count={count}");
        let headers = self.auth_headers()?;
        let response = self.send_get_request("share.list", &url, &headers).await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("share.list", response, "List shares failed").await);
        }
        let payload: types::ApiResponse<types::ShareListData> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Get share content by share ID.
    pub async fn get_share_content(&self, share_id: &str) -> AppResult<types::ShareContentData> {
        let url = format!("{SHARE_CONTENT_URL}?share_id={share_id}");
        let headers = self.auth_headers()?;
        let response = self.send_get_request("share.content", &url, &headers).await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("share.content", response, "Get share content failed").await);
        }
        let payload: types::ApiResponse<types::ShareContentData> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Delete a share.
    pub async fn delete_share(&self, share_id: &str) -> AppResult<()> {
        let body = json!({ "share_id": share_id });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("share.delete", DELETE_SHARE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("share.delete", response, "Delete share failed").await);
        }
        Ok(())
    }

    /// Fork a shared conversation into a new session.
    pub async fn fork_share(&self, share_id: &str) -> AppResult<types::ForkShareData> {
        let body = json!({ "share_id": share_id });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("share.fork", FORK_SHARE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("share.fork", response, "Fork share failed").await);
        }
        let payload: types::ApiResponse<types::ForkShareData> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    // ─── Search ────────────────────────────────────────────────

    /// Prepare the search index for a session (or all sessions).
    pub async fn prepare_search_index(
        &self,
        session_id: Option<&str>,
    ) -> AppResult<()> {
        let body = match session_id {
            Some(sid) => json!({ "chat_session_id": sid }),
            None => json!({}),
        };
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("index.prepare", INDEX_PREPARE_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("index.prepare", response, "Prepare index failed").await);
        }
        Ok(())
    }

    /// Query the conversation search index.
    pub async fn query_search_index(
        &self,
        query: &str,
        limit: Option<i64>,
    ) -> AppResult<types::IndexQueryData> {
        let body = json!({
            "query": query,
            "limit": limit,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("index.query", INDEX_QUERY_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("index.query", response, "Query index failed").await);
        }
        let payload: types::ApiResponse<types::IndexQueryData> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    // ─── User ──────────────────────────────────────────────────

    /// Get the current authenticated user's information.
    pub async fn get_current_user(&self) -> AppResult<types::DeepSeekUser> {
        let headers = self.auth_headers()?;
        let response = self
            .send_get_request("user.current", CURRENT_USER_URL, &headers)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("user.current", response, "Get current user failed").await);
        }
        let payload: types::ApiResponse<types::DeepSeekUser> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Get user settings (training_allowed flag).
    pub async fn get_user_settings(&self) -> AppResult<types::UserSettings> {
        let headers = self.auth_headers()?;
        let response = self
            .send_get_request("user.settings", USER_SETTINGS_URL, &headers)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("user.settings", response, "Get user settings failed").await);
        }
        let payload: types::ApiResponse<types::UserSettings> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Update user settings.
    pub async fn update_user_settings(&self, settings: serde_json::Value) -> AppResult<types::UserSettings> {
        let body = json!({ "settings": settings });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("user.update_settings", UPDATE_USER_SETTINGS_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("user.update_settings", response, "Update settings failed").await);
        }
        let payload: types::ApiResponse<types::UserSettings> =
            response.json().await?;
        Ok(payload.data.biz_data)
    }

    /// Logout all active sessions.
    pub async fn logout_all_sessions(&self) -> AppResult<()> {
        let body = json!({});
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("user.logout_all", LOGOUT_ALL_SESSIONS_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("user.logout_all", response, "Logout all failed").await);
        }
        Ok(())
    }

    /// Set user birthday.
    pub async fn set_birthday(&self, birthday: &str) -> AppResult<()> {
        let body = json!({ "birthday": birthday });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("user.set_birthday", SET_BIRTHDAY_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("user.set_birthday", response, "Set birthday failed").await);
        }
        Ok(())
    }

    // ─── Client Settings ──────────────────────────────────────

    /// Fetch client settings for a given scope.
    pub async fn get_client_settings(
        &self,
        did: &str,
        scope: &str,
    ) -> AppResult<serde_json::Value> {
        let url = format!("{CLIENT_SETTINGS_URL}?did={}&scope={}", urlencoding(did), urlencoding(scope));
        let headers = self.auth_headers()?;
        let response = self
            .send_get_request("client.settings", &url, &headers)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("client.settings", response, "Get client settings failed").await);
        }
        let payload: Value = response.json().await?;
        Ok(payload)
    }

    /// Report client settings.
    pub async fn report_client_settings(
        &self,
        settings_ids: &[i64],
        did: &str,
        sso_id: &str,
    ) -> AppResult<()> {
        let body = json!({
            "settings_ids": settings_ids,
            "did": did,
            "sso_id": sso_id,
        });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request("client.settings.report", CLIENT_SETTINGS_REPORT_URL, &headers, &body)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response("client.settings.report", response, "Report settings failed").await);
        }
        Ok(())
    }
}

/// Simple URL encoding for query parameters.
fn urlencoding(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push_str("%20"),
            _ => {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    result
}

#[async_trait]
impl LLMProvider for DeepseekProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let (response, session_id) = self.send_request(&request).await?;
        let mut stream = response.bytes_stream();
        let mut sse = super::sse::SseLineBuffer::new();
        let mut content = String::new();
        let mut finish_reason = None;
        let mut parent_message_id = None;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    // Persist the thread id before bailing so an interrupted
                    // collection doesn't fork the conversation on the next turn.
                    let _ = self.mark_session_after_success(&session_id, parent_message_id);
                    return Err(error.into());
                }
            };

            for line in sse.push_bytes(&chunk) {
                let trimmed = line.trim();
                if trimmed == "data: [DONE]" {
                    finish_reason = Some("stop".to_string());
                    debug_log::log("completion.collect.done", "received [DONE]");
                    self.mark_session_after_success(&session_id, parent_message_id)?;
                    return Ok(CompletionResponse {
                        content,
                        finish_reason,
                        usage: None,
                    });
                }

                if let Some((text_chunk, maybe_parent_id)) = Self::process_stream_line(&line) {
                    debug_log::log("completion.collect.line", line.trim());
                    if let Some(text) = text_chunk {
                        debug_log::log(
                            "completion.collect.chunk",
                            format!("text_chunk={}", text),
                        );
                        content.push_str(&text);
                    }
                    if maybe_parent_id.is_some() {
                        parent_message_id = maybe_parent_id;
                        debug_log::log(
                            "completion.collect.parent",
                            format!("parent_message_id={:?}", parent_message_id),
                        );
                        // Persist immediately so a cut stream keeps the thread id.
                        let _ = self.mark_session_after_success(&session_id, parent_message_id);
                    }
                }
            }
        }

        self.mark_session_after_success(&session_id, parent_message_id)?;
        Ok(CompletionResponse {
            content,
            finish_reason,
            usage: None,
        })
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        let (response, session_id) = self.send_request(&request).await?;
        let mut stream = response.bytes_stream();
        let mut sse = super::sse::SseLineBuffer::new();
        let mut parent_message_id = None;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    // The server already advanced this session's message tree;
                    // persist the id we have so the next message threads onto it
                    // instead of forking onto an invisible branch.
                    let _ = self.mark_session_after_success(&session_id, parent_message_id);
                    return Err(error.into());
                }
            };

            for line in sse.push_bytes(&chunk) {
                let trimmed = line.trim();
                if trimmed == "data: [DONE]" {
                    debug_log::log("completion.stream.done", "received [DONE]");
                    self.mark_session_after_success(&session_id, parent_message_id)?;
                    let _ = tx.send(CompletionChunk {
                        content: String::new(),
                        finish_reason: Some("stop".to_string()),
                    });
                    return Ok(());
                }

                if let Some((text_chunk, maybe_parent_id)) = Self::process_stream_line(&line) {
                    debug_log::log("completion.stream.line", line.trim());
                    if maybe_parent_id.is_some() {
                        parent_message_id = maybe_parent_id;
                        debug_log::log(
                            "completion.stream.parent",
                            format!("parent_message_id={:?}", parent_message_id),
                        );
                        // Persist immediately — if the stream is cut after this,
                        // the thread id is already saved.
                        let _ = self.mark_session_after_success(&session_id, parent_message_id);
                    }

                    if let Some(text) = text_chunk {
                        debug_log::log(
                            "completion.stream.chunk",
                            format!("text_chunk={}", text),
                        );
                        let _ = tx.send(CompletionChunk {
                            content: text,
                            finish_reason: None,
                        });
                    }
                }
            }
        }

        self.mark_session_after_success(&session_id, parent_message_id)?;
        Ok(())
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn reset(&self) -> AppResult<()> {
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
        *state = SessionState::default();
        Ok(())
    }

    async fn fetch_remote_session_messages(
        &self,
        session_id: &str,
    ) -> AppResult<Vec<ChatMessage>> {
        self.fetch_remote_history(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderConfig, ProviderKind};

    fn provider() -> DeepseekProvider {
        // Builds a reqwest client only — no network, no token needed.
        let config = ProviderConfig {
            kind: ProviderKind::Deepseek,
            token: String::new(),
            model: "deepseek-chat".to_string(),
            base_url: None,
            temperature: 0.0,
            max_tokens: 128,
        };
        DeepseekProvider::new(&config, 0, 0).expect("client builds")
    }

    #[test]
    fn parent_id_persists_once_recorded_and_survives_empty_marks() {
        let p = provider();
        p.session_state.lock().unwrap().session_id = Some("sess-1".to_string());

        // A message id seen mid-stream is recorded immediately.
        p.mark_session_after_success("sess-1", Some(42)).unwrap();
        {
            let s = p.session_state.lock().unwrap();
            assert_eq!(s.parent_message_id, Some(42));
            assert!(s.system_sent_for_session);
        }

        // A later mark with no new id (clean end, or interrupted stream) must
        // NOT clobber the recorded thread id — this is exactly what keeps an
        // abnormally-ended turn from forking the conversation onto a branch the
        // web UI never shows.
        p.mark_session_after_success("sess-1", None).unwrap();
        assert_eq!(p.session_state.lock().unwrap().parent_message_id, Some(42));
    }

    #[test]
    fn mark_ignores_a_stale_session_id() {
        let p = provider();
        {
            let mut s = p.session_state.lock().unwrap();
            s.session_id = Some("current".to_string());
            s.parent_message_id = Some(7);
        }
        // A late event referring to a previous/reset session must not rewrite
        // the current session's thread id.
        p.mark_session_after_success("old-session", Some(999)).unwrap();
        assert_eq!(p.session_state.lock().unwrap().parent_message_id, Some(7));
    }
}
