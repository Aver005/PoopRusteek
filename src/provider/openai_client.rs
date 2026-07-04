//! `LLMProvider` over any OpenAI-compatible endpoint (LM Studio, Ollama's
//! `/v1`, vLLM, OpenRouter, …). The wire format lives in
//! [`super::openai_compat`]; this file is only transport: one POST per
//! turn, SSE line reassembly via [`super::sse::SseLineBuffer`].
//!
//! Deliberately stateless — OpenAI-style APIs carry the whole history in
//! every request, so there is no session id, no `parent_message_id`
//! threading, and `fork()` is a plain config copy. All the session-related
//! `LLMProvider` methods keep their "unsupported" defaults.

use super::openai_compat::{self, ChatCompletionChunk, ChatCompletionResponse};
use super::sse::SseLineBuffer;
use super::{ChatMessage, CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider};
use crate::config::ProviderEntry;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    /// Base URL without a trailing slash (usually `…/v1`).
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OpenAiCompatProvider {
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
        })
    }

    fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    async fn send(&self, request: &CompletionRequest, stream: bool) -> AppResult<reqwest::Response> {
        let mut body = openai_compat::request_to_openai(request);
        body["stream"] = serde_json::json!(stream);
        // The entry's model wins over whatever the internal request carries —
        // the TUI's model field is a DeepSeek-ism ("deepseek-chat"/"expert").
        body["model"] = serde_json::json!(self.model);

        let mut http_request = self.client.post(self.completions_url()).json(&body);
        if let Some(key) = &self.api_key {
            http_request = http_request.bearer_auth(key);
        }
        debug_log::log(
            "openai_compat.request",
            format!("url={} stream={stream} model={}", self.completions_url(), self.model),
        );

        let response = http_request.send().await.map_err(AppError::Http)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            // Surface the OpenAI error envelope's message when there is one;
            // fall back to the raw (truncated) body.
            let message = serde_json::from_str::<openai_compat::ErrorResponse>(&text)
                .map(|envelope| envelope.error.message)
                .unwrap_or_else(|_| crate::util::truncate_at_char_boundary(&text, 300).to_string());
            debug_log::log("openai_compat.error", format!("status={status} message={message}"));
            return Err(AppError::Provider(format!("{status}: {message}")));
        }
        Ok(response)
    }
}

#[async_trait]
impl LLMProvider for OpenAiCompatProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let response = self.send(&request, false).await?;
        let parsed: ChatCompletionResponse = response.json().await.map_err(AppError::Http)?;
        Ok(openai_compat::response_from_openai(parsed))
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
                let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                    continue; // comments / event: lines — not part of this protocol's data
                };
                if payload == "[DONE]" {
                    return Ok(());
                }
                match serde_json::from_str::<ChatCompletionChunk>(payload) {
                    Ok(chunk) => {
                        let _ = tx.send(openai_compat::chunk_from_openai(chunk));
                    }
                    Err(error) => {
                        // One malformed chunk shouldn't kill the stream, but
                        // it must not vanish silently either.
                        debug_log::log(
                            "openai_compat.stream.parse",
                            format!("skipping malformed chunk: {error}"),
                        );
                    }
                }
            }
        }
        // Stream ended without `[DONE]` — the runner treats a missing stop
        // chunk as a provider error, which is exactly right here too.
        Ok(())
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
        })
    }

    async fn fetch_remote_session_messages(
        &self,
        _session_id: &str,
    ) -> AppResult<Vec<ChatMessage>> {
        Err(AppError::Custom(
            "Remote sessions are a DeepSeek-provider feature".to_string(),
        ))
    }
}
