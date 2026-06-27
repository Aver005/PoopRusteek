use super::pow;
use super::*;
use crate::config::ProviderConfig;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use regex::Regex;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Response,
};
use serde_json::{json, Value};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const DEEPSEEK_HOST: &str = "chat.deepseek.com";
const CREATE_POW_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
const SESSION_HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history";
const TARGET_PATH: &str = "/api/v0/chat/completion";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 YaBrowser/26.3.0.0 Safari/537.36";

static LONG_CODE_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)```.{300,}?```").expect("hardcoded regex is valid")
});

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

    fn split_system_prompt(messages: &[ChatMessage]) -> (String, Vec<ChatMessage>) {
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
            let stripped = Self::strip_long_code_blocks(message.content.trim());
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

    fn build_prompt(
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
                    .map(Self::format_history_message)
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

    fn resolve_model_type(model: &str, parent_message_id: Option<i64>) -> Option<&'static str> {
        let lower = model.to_ascii_lowercase();
        if lower.contains("reasoner") || lower.contains("expert") {
            Some("expert")
        } else if parent_message_id.is_none() {
            Some("default")
        } else {
            None
        }
    }

    fn build_body(
        &self,
        request: &CompletionRequest,
        prompt: String,
        session: &SessionSnapshot,
    ) -> Value {
        let model_type = Self::resolve_model_type(&request.model, session.parent_message_id);
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
        let (system_prompt, non_system_messages) = Self::split_system_prompt(&request.messages);
        let should_reset = {
            let state = self
                .session_state
                .lock()
                .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
            state.system_sent_for_session && non_system_messages.len() == 1
        };

        let session = self.ensure_session(should_reset).await?;
        let prompt = Self::build_prompt(
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
                messages.push(ChatMessage { role, content, name: None, tool_call_id: None, display_content: None, tool_error: false, created_at: String::new(), total_tokens: None });
            }
        }
        Ok(messages)
    }
}

#[async_trait]
impl LLMProvider for DeepseekProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let (response, session_id) = self.send_request(&request).await?;
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut content = String::new();
        let mut finish_reason = None;
        let mut parent_message_id = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].to_string();
                buffer = buffer[line_end + 1..].to_string();

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
        let mut buffer = String::new();
        let mut parent_message_id = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].to_string();
                buffer = buffer[line_end + 1..].to_string();

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
