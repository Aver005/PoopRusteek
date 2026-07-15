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

use super::sse::{SseLineBuffer, sse_data_payload};
use super::{CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider, Role};
use crate::config::ProviderEntry;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use std::marker::PhantomData;
use std::sync::Arc;

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

    /// Interpret one SSE `data:` payload: emit zero or more chunks through
    /// `emit` (Gemini's last payload carries text *and* the finish), then
    /// tell the pump how to proceed.
    fn handle_sse_payload(payload: &str, emit: &mut dyn FnMut(CompletionChunk)) -> SseFlow;

    /// Parse the model-listing response body into model ids.
    fn parse_models(body: &[u8]) -> AppResult<Vec<String>>;
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
            Role::System => {
                system_parts.push(&message.content);
                continue;
            }
            Role::User => ("user", message.content.clone()),
            Role::Assistant => (assistant_role, message.content.clone()),
            // Results go back as user text, labeled so the model can tell
            // them from the human.
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
    _protocol: PhantomData<P>,
}

impl<P: CompatProtocol> CompatClient<P> {
    pub fn new(entry: &ProviderEntry) -> AppResult<Self> {
        // Same envelope as the DeepSeek client: fail fast on dead endpoints,
        // and let the agent loop's 120s idle guard own the stall policy.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(AppError::Http)?;
        Ok(Self {
            client,
            base_url: entry.base_url.trim_end_matches('/').to_string(),
            api_key: entry.api_key.clone(),
            model: entry.model.clone(),
            _protocol: PhantomData,
        })
    }

    async fn send(
        &self,
        request: &CompletionRequest,
        stream: bool,
    ) -> AppResult<reqwest::Response> {
        let body = P::request_body(request, &self.model, stream);
        let url = P::completions_url(&self.base_url, &self.model, stream);
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
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = P::error_message(&text)
                .unwrap_or_else(|| crate::util::truncate_at_char_boundary(&text, 300).to_string());
            debug_log::log(
                &format!("{}.error", P::LOG_TAG),
                format!("status={status} message={message}"),
            );
            return Err(AppError::Provider(format!("{status}: {message}")));
        }
        Ok(response)
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

        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(AppError::Http)?;
            for line in buffer.push_bytes(&piece) {
                // Comments / `event:` lines carry nothing these protocols
                // need — every payload of interest arrives on a `data:` line.
                let Some(payload) = sse_data_payload(&line) else {
                    continue;
                };
                match P::handle_sse_payload(payload, &mut |chunk| {
                    let _ = tx.send(chunk);
                }) {
                    SseFlow::Continue => {}
                    SseFlow::Done => return Ok(()),
                    SseFlow::Fail(message) => return Err(AppError::Provider(message)),
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
        if !status.is_success() {
            return Err(AppError::Provider(format!("GET /models failed: {status}")));
        }
        let body = response.bytes().await.map_err(AppError::Http)?;
        P::parse_models(&body)
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn fork(&self) -> Arc<dyn LLMProvider> {
        // Stateless: a fork is just another handle onto the same endpoint.
        Arc::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            _protocol: PhantomData,
        })
    }
}
