//! `LLMProvider` over the Google Generative Language API (Gemini).
//! Transport triplet-sibling of [`super::openai_client`] and
//! [`super::anthropic_client`]: wire format in [`super::gemini_compat`],
//! this file only does HTTP + SSE.
//!
//! Gemini quirks the transport owns: the model id lives in the URL
//! (`{base}/models/{model}:generateContent`), auth is the `x-goog-api-key`
//! header, and the SSE stream (`:streamGenerateContent?alt=sse`) has no
//! `[DONE]` terminator — the last chunk carries `finishReason` instead.
//! Stateless, like every compat provider.

use super::gemini_compat::{self, GenerateContentResponse};
use super::sse::SseLineBuffer;
use super::{CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider};
use crate::config::ProviderEntry;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;

pub struct GeminiProvider {
    client: reqwest::Client,
    /// Base URL without a trailing slash (usually `…/v1beta`).
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl GeminiProvider {
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

    fn with_key(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.header("x-goog-api-key", key),
            None => request,
        }
    }

    async fn send(
        &self,
        request: &CompletionRequest,
        stream: bool,
    ) -> AppResult<reqwest::Response> {
        let body = gemini_compat::request_to_gemini(request);
        let url = if stream {
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.base_url, self.model
            )
        } else {
            format!("{}/models/{}:generateContent", self.base_url, self.model)
        };
        debug_log::log(
            "gemini.request",
            format!("url={url} stream={stream} model={}", self.model),
        );
        let response = self
            .with_key(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .map_err(AppError::Http)?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            // Google's error envelope is {"error": {"message": ..., ...}}.
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| value["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| crate::util::truncate_at_char_boundary(&text, 300).to_string());
            debug_log::log("gemini.error", format!("status={status} message={message}"));
            return Err(AppError::Provider(format!("{status}: {message}")));
        }
        Ok(response)
    }
}

#[async_trait]
impl LLMProvider for GeminiProvider {
    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let response = self.send(&request, false).await?;
        let parsed: GenerateContentResponse = response.json().await.map_err(AppError::Http)?;
        Ok(gemini_compat::response_from_gemini(parsed))
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
                    continue;
                };
                let Ok(chunk) = serde_json::from_str::<GenerateContentResponse>(payload) else {
                    debug_log::log("gemini.stream.parse", "skipping malformed chunk");
                    continue;
                };
                let (text, finish) = gemini_compat::extract_piece(&chunk);
                if !text.is_empty() {
                    let _ = tx.send(CompletionChunk {
                        content: text,
                        finish_reason: None,
                    });
                }
                if let Some(reason) = finish {
                    let _ = tx.send(CompletionChunk {
                        content: String::new(),
                        finish_reason: Some(reason),
                    });
                    return Ok(());
                }
            }
        }
        // Ended without a finishReason — the runner's no-stop error path
        // handles it.
        Ok(())
    }

    async fn list_models(&self) -> AppResult<Vec<String>> {
        let response = self
            .with_key(self.client.get(format!("{}/models", self.base_url)))
            .send()
            .await
            .map_err(AppError::Http)?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Provider(format!("GET /models failed: {status}")));
        }
        let page: gemini_compat::ModelsPage = response.json().await.map_err(AppError::Http)?;
        Ok(page
            .models
            .into_iter()
            .map(|model| model.name.trim_start_matches("models/").to_string())
            .collect())
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
