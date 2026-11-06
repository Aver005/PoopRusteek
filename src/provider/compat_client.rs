//! Shared transport for every stateless "compat" provider (OpenAI-style,
//! Anthropic-style, Gemini).
//!
//! The three protocols differ only in wire details — endpoint shape, auth
//! headers, body tweaks, error envelope, SSE payload interpretation, model
//! list shape — so those live behind [`CompatProtocol`] as zero-sized
//! plug-ins, and everything an HTTP turn shares (client construction, the
//! request/error envelope, the SSE pump, `fork()`) lives once in
//! [`CompatClient`]. Before this module the whole transport was
//! copy-pasted per provider (~170 lines each, byte-identical apart from
//! the protocol hooks).
//!
//! Stateless by contract: these APIs carry the whole history in every
//! request, so there is no session id, no `parent_message_id` threading,
//! and `fork()` is a plain config copy. All session-related `LLMProvider`
//! methods keep their "unsupported" trait defaults.

use super::failure::{ProviderFailure, RateLimitHeaders};
use super::sse::{SseLineBuffer, sse_data_payload};
use super::{CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider, Role};
use crate::config::ProviderEntry;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What [`CompatProtocol::handle_sse_payload`] tells the pump to do after
/// interpreting one `data:` payload.
pub(crate) enum SseFlow {
    /// Keep reading the stream.
    Continue,
    /// Terminal payload seen — end the stream successfully.
    Done,
    /// Fatal in-stream error — abort with a provider error.
    Fail(String),
}

/// One wire protocol plugged into [`CompatClient`].
///
/// Implementations are zero-sized: every hook is an associated function
/// over plain data, so a protocol carries no state and costs nothing per
/// instance — the client's config (base URL, key, model) is passed in.
pub(crate) trait CompatProtocol: Send + Sync + 'static {
    /// `debug_log` event prefix (`{TAG}.request`, `{TAG}.error`).
    const LOG_TAG: &'static str;

    /// Completion endpoint. `model`/`stream` matter only to protocols that
    /// encode them in the URL (Gemini); the others ignore them.
    fn completions_url(base_url: &str, model: &str, stream: bool) -> String;

    /// Model-listing endpoint (`GET`).
    fn models_url(base_url: &str) -> String {
        format!("{base_url}/models")
    }

    /// Attach auth (and any protocol-pinned headers) to a request.
    fn apply_headers(
        request: reqwest::RequestBuilder,
        api_key: Option<&str>,
    ) -> reqwest::RequestBuilder;

    /// Build the JSON body for a completion call.
    fn request_body(request: &CompletionRequest, model: &str, stream: bool) -> serde_json::Value;

    /// Pull the human-readable message out of an error response body, or
    /// `None` to fall back to the (truncated) raw body.
    fn error_message(body: &str) -> Option<String>;

    /// Parse a blocking completion response body.
    fn parse_response(body: &[u8]) -> AppResult<CompletionResponse>;

    /// Что протокол копит между событиями одного стрима: вызовы инструментов
    /// приходят кусками и отдаются наверх только целыми.
    type StreamState: Default + Send;

    /// Interpret one SSE `data:` payload: emit zero or more chunks through
    /// `emit` (Gemini's last payload carries text *and* the finish), then
    /// tell the pump how to proceed.
    fn handle_sse_payload(
        payload: &str,
        state: &mut Self::StreamState,
        emit: &mut dyn FnMut(CompletionChunk),
    ) -> SseFlow;

    /// Parse the model-listing response body into model ids.
    fn parse_models(body: &[u8]) -> AppResult<Vec<String>>;

    /// Заголовки лимитов ответа. У протокола без них — `None`.
    fn parse_limits(
        _headers: &reqwest::header::HeaderMap,
        _now: std::time::SystemTime,
    ) -> Option<RateLimitHeaders> {
        None
    }

    /// Попросить сервер прислать расход в потоке (OpenAI: `stream_options`).
    fn request_stream_usage(_body: &mut serde_json::Value) {}

    /// Отверг ли сервер этот параметр: тогда запрос повторяется без него.
    fn rejects_stream_usage(_status: u16, _message: &str) -> bool {
        false
    }
}

/// Кусок потока не разобрался: это ошибка в конверте `{"error": …}` — конец
/// потока с её текстом, — или просто битый кусок, который пропускаем.
pub(crate) fn unparsed_payload(tag: &str, payload: &str, error: &str) -> SseFlow {
    if let Some(message) = envelope_error_message(payload) {
        return SseFlow::Fail(message);
    }
    debug_log::log(
        &format!("{tag}.stream.parse"),
        format!("skipping malformed chunk: {error}"),
    );
    SseFlow::Continue
}

/// `{"error": {"message": …}}` — the error envelope Anthropic and Google
/// happen to share, byte-identically parsed before this helper existed.
pub(crate) fn envelope_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_string))
}

/// Collapse an internal message history into the strictly-alternating
/// `(role, text)` turn list the Anthropic and Gemini APIs both require:
/// `ui_only` chrome dropped, system messages collected separately, tool
/// results relabeled as user text (prompt-encoded tool protocol), and
/// consecutive same-role runs merged; a `(continue)` user opener is
/// inserted when the transcript would start on the model's side, which
/// both APIs reject. `assistant_role` is the protocol's name for the model
/// side (`"assistant"` / `"model"`) — the only thing that differed between
/// the two previously copy-pasted implementations.
pub(crate) fn merge_alternating_turns<'m>(
    request: &'m CompletionRequest,
    assistant_role: &'static str,
) -> (Vec<&'m str>, Vec<(&'static str, String)>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut turns: Vec<(&'static str, String)> = Vec::new();

    for message in request.messages.iter().filter(|message| !message.ui_only) {
        let (role, text) = match message.role {
            // Системное в начале — system prompt; заметка посреди истории
            // остаётся на своём месте текстом.
            Role::System if turns.is_empty() => {
                system_parts.push(&message.content);
                continue;
            }
            Role::System => ("user", message.inline_note()),
            Role::User => ("user", message.content.clone()),
            Role::Assistant => (assistant_role, message.content.clone()),
            // Results go back as user text, labeled so the model can tell
            // them from the human.
            Role::Tool => (
                "user",
                format!("[tool result]\n{}", message.labelled_tool_output()),
            ),
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

    if turns
        .first()
        .is_none_or(|(role, _)| *role == assistant_role)
    {
        turns.insert(0, ("user", "(continue)".to_string()));
    }

    (system_parts, turns)
}

/// The shared HTTP/SSE transport; `P` contributes only the wire details.
pub(crate) struct CompatClient<P: CompatProtocol> {
    client: reqwest::Client,
    /// Base URL without a trailing slash (usually `…/v1` / `…/v1beta`).
    base_url: String,
    api_key: Option<String>,
    model: String,
    /// Запись объявляет инструменты родным протоколом (`tools = "native"`).
    native_tools: bool,
    /// Заголовки лимитов последнего ответа; общие для форков одного аккаунта.
    limits: Arc<Mutex<Option<RateLimitHeaders>>>,
    /// Просить ли расход в потоке. Падает в `false`, когда сервер отверг параметр.
    stream_usage: Arc<AtomicBool>,
    _protocol: PhantomData<P>,
}

impl<P: CompatProtocol> CompatClient<P> {
    pub fn new(entry: &ProviderEntry) -> AppResult<Self> {
        // Same envelope as the DeepSeek client: fail fast on dead endpoints;
        // a stalled read is the provider's error, named before the idle guard.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(crate::provider::PROVIDER_READ_TIMEOUT)
            .build()
            .map_err(AppError::Http)?;
        Ok(Self {
            client,
            base_url: entry.base_url.trim_end_matches('/').to_string(),
            api_key: entry.api_key.clone(),
            model: entry.model.clone(),
            native_tools: entry.tools == crate::config::ToolProtocol::Native,
            limits: Arc::new(Mutex::new(None)),
            stream_usage: Arc::new(AtomicBool::new(true)),
            _protocol: PhantomData,
        })
    }

    /// Запомнить свежие заголовки лимитов (замок держится без `.await`).
    fn remember_limits(&self, limits: Option<RateLimitHeaders>) {
        if let Some(limits) = limits
            && let Ok(mut slot) = self.limits.lock()
        {
            *slot = Some(limits);
        }
    }

    async fn send(
        &self,
        request: &CompletionRequest,
        stream: bool,
    ) -> AppResult<reqwest::Response> {
        let url = P::completions_url(&self.base_url, &self.model, stream);
        let mut ask_usage = stream && self.stream_usage.load(Ordering::Relaxed);
        loop {
            let mut body = P::request_body(request, &self.model, stream);
            if ask_usage {
                P::request_stream_usage(&mut body);
            }
            debug_log::log(
                &format!("{}.request", P::LOG_TAG),
                format!("url={url} stream={stream} model={}", self.model),
            );
            let response = P::apply_headers(self.client.post(&url), self.api_key.as_deref())
                .json(&body)
                .send()
                .await
                .map_err(AppError::Http)?;

            let status = response.status();
            let headers = response.headers().clone();
            let limits = P::parse_limits(&headers, std::time::SystemTime::now())
                .filter(|limits| !limits.is_empty());
            self.remember_limits(limits.clone());
            if status.is_success() {
                return Ok(response);
            }
            let text = response.text().await.unwrap_or_default();
            let message = P::error_message(&text)
                .unwrap_or_else(|| crate::util::truncate_at_char_boundary(&text, 300).to_string());
            debug_log::log(
                &format!("{}.error", P::LOG_TAG),
                format!("status={status} message={message}"),
            );
            // Сервер не знает параметра расхода: повторяем без него и больше не просим.
            if ask_usage && P::rejects_stream_usage(status.as_u16(), &message) {
                self.stream_usage.store(false, Ordering::Relaxed);
                ask_usage = false;
                continue;
            }
            return Err(AppError::Upstream(Box::new(
                ProviderFailure::from_response(status, &headers, &text, message, limits),
            )));
        }
    }
}

#[async_trait]
impl<P: CompatProtocol> LLMProvider for CompatClient<P> {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let response = self.send(&request, false).await?;
        let body = response.bytes().await.map_err(AppError::Http)?;
        P::parse_response(&body)
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        let response = self.send(&request, true).await?;
        let mut stream = response.bytes_stream();
        let mut buffer = SseLineBuffer::new();
        let mut state = P::StreamState::default();

        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(AppError::Http)?;
            for line in buffer.push_bytes(&piece) {
                // Comments / `event:` lines carry nothing these protocols
                // need — every payload of interest arrives on a `data:` line.
                let Some(payload) = sse_data_payload(&line) else {
                    continue;
                };
                match P::handle_sse_payload(payload, &mut state, &mut |chunk| {
                    let _ = tx.send(chunk);
                }) {
                    SseFlow::Continue => {}
                    SseFlow::Done => return Ok(()),
                    SseFlow::Fail(message) => {
                        return Err(AppError::Upstream(Box::new(ProviderFailure::in_stream(
                            message,
                        ))));
                    }
                }
            }
        }
        // Stream ended without the protocol's terminal signal — the runner
        // treats a missing stop chunk as a provider error, which is exactly
        // right here too.
        Ok(())
    }

    async fn list_models(&self) -> AppResult<Vec<String>> {
        let response = P::apply_headers(
            self.client.get(P::models_url(&self.base_url)),
            self.api_key.as_deref(),
        )
        .send()
        .await
        .map_err(AppError::Http)?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message =
                P::error_message(&text).unwrap_or_else(|| format!("GET /models failed: {status}"));
            return Err(AppError::Upstream(Box::new(
                ProviderFailure::from_response(status, &headers, &text, message, None),
            )));
        }
        let body = response.bytes().await.map_err(AppError::Http)?;
        P::parse_models(&body)
    }

    /// OpenAI's `/models` schema has no window field, but OpenRouter and
    /// several compatible gateways add `context_length` — read it when it is
    /// there, stay silent when it is not. LM Studio and Ollama expose theirs
    /// on non-OpenAI routes (`/api/v0/models`, `/api/show`) and are not
    /// covered here.
    async fn context_window(&self) -> Option<u32> {
        let response = P::apply_headers(
            self.client.get(P::models_url(&self.base_url)),
            self.api_key.as_deref(),
        )
        .send()
        .await
        .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        body.get("data")?
            .as_array()?
            .iter()
            .find(|entry| entry.get("id").and_then(|id| id.as_str()) == Some(self.model.as_str()))?
            .get("context_length")?
            .as_u64()?
            .try_into()
            .ok()
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn native_tools(&self) -> bool {
        self.native_tools
    }

    fn rate_limits(&self) -> Option<RateLimitHeaders> {
        self.limits.lock().ok().and_then(|slot| slot.clone())
    }

    fn fork(&self) -> Arc<dyn LLMProvider> {
        // Stateless: a fork is just another handle onto the same endpoint.
        Arc::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            native_tools: self.native_tools,
            limits: Arc::clone(&self.limits),
            stream_usage: Arc::clone(&self.stream_usage),
            _protocol: PhantomData,
        })
    }
}

#[cfg(test)]
mod stream_error_tests {
    use super::*;

    /// Ошибка внутри потока раньше пропускалась как битый кусок, и её текст
    /// оставался только в журнале.
    #[test]
    fn an_in_stream_error_ends_the_stream_with_its_text() {
        let flow = unparsed_payload(
            "openai_compat",
            r#"{"error":{"message":"quota exceeded"}}"#,
            "missing field",
        );
        assert!(matches!(flow, SseFlow::Fail(message) if message == "quota exceeded"));
        assert!(matches!(
            unparsed_payload("openai_compat", "{garbage", "eof"),
            SseFlow::Continue
        ));
    }
}
