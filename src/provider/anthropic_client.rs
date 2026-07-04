//! `LLMProvider` over any Anthropic-compatible endpoint (the Anthropic API
//! itself, Claude-compatible proxies/gateways). Transport twin of
//! [`super::openai_client`]: the wire format lives in
//! [`super::anthropic_compat`], this file only does HTTP + SSE.
//!
//! Auth follows the Anthropic convention: the key goes in `x-api-key`
//! (not a bearer header) alongside a pinned `anthropic-version`.
//! Stateless, like every compat provider: full history per request,
//! `fork()` is a config copy.

use super::anthropic_compat::{self, StreamEvent};
use super::sse::SseLineBuffer;
use super::{CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider};
use crate::config::ProviderEntry;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;

/// The Messages API requires this header; compat servers accept it.
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicCompatProvider {
    client: reqwest::Client,
    /// Base URL without a trailing slash (usually `…/v1`).
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl AnthropicCompatProvider {
    pub fn new(entry: &ProviderEntry) -> AppResult<Self> {
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
        })
    }

    fn with_headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header("anthropic-version", ANTHROPIC_VERSION);
        match &self.api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    async fn send(&self, request: &CompletionRequest, stream: bool) -> AppResult<reqwest::Response> {
        let mut body = anthropic_compat::request_to_anthropic(request);
        body["stream"] = serde_json::json!(stream);
        body["model"] = serde_json::json!(self.model);

        let url = format!("{}/messages", self.base_url);
        debug_log::log(
            "anthropic_compat.request",
            format!("url={url} stream={stream} model={}", self.model),
        );
        let response = self
            .with_headers(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .map_err(AppError::Http)?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            // Anthropic's error envelope is {"type":"error","error":{...}}.
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| {
                    value["error"]["message"].as_str().map(str::to_string)
                })
                .unwrap_or_else(|| crate::util::truncate_at_char_boundary(&text, 300).to_string());
            debug_log::log("anthropic_compat.error", format!("status={status} message={message}"));
            return Err(AppError::Provider(format!("{status}: {message}")));
        }
        Ok(response)
    }
}

#[async_trait]
impl LLMProvider for AnthropicCompatProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let response = self.send(&request, false).await?;
        let parsed: anthropic_compat::MessagesResponse =
            response.json().await.map_err(AppError::Http)?;
        Ok(anthropic_compat::response_from_anthropic(parsed))
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
                // `event: <type>` lines are redundant — the type is
                // repeated inside each data payload.
                let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                match anthropic_compat::parse_stream_event(payload) {
                    StreamEvent::Text(text) => {
                        let _ = tx.send(CompletionChunk { content: text, finish_reason: None });
                    }
                    StreamEvent::Done => {
                        let _ = tx.send(CompletionChunk {
                            content: String::new(),
                            finish_reason: Some("stop".to_string()),
                        });
                        return Ok(());
                    }
                    StreamEvent::Error(message) => {
                        return Err(AppError::Provider(message));
                    }
                    StreamEvent::Ignore => {}
                }
            }
        }
        // Ended without message_stop — the runner's no-stop error path
        // handles it.
        Ok(())
    }

    async fn list_models(&self) -> AppResult<Vec<String>> {
        let response = self
            .with_headers(self.client.get(format!("{}/models", self.base_url)))
            .send()
            .await
            .map_err(AppError::Http)?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Provider(format!("GET /models failed: {status}")));
        }
        let page: anthropic_compat::ModelsPage = response.json().await.map_err(AppError::Http)?;
        Ok(page.data.into_iter().map(|model| model.id).collect())
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn fork(&self) -> Arc<dyn LLMProvider> {
        Arc::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
        })
    }
}
