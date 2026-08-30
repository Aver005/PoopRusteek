//! Gemini (Google Generative Language API) protocol plug-in for the shared
//! [`CompatClient`] transport. The wire format lives in
//! [`super::gemini_compat`]; this file contributes only the protocol
//! details.
//!
//! Gemini quirks this plug-in owns: the model id lives in the URL
//! (`{base}/models/{model}:generateContent`), auth is the `x-goog-api-key`
//! header, and the SSE stream (`:streamGenerateContent?alt=sse`) has no
//! `[DONE]` terminator — the last chunk carries `finishReason` instead.

use super::compat_client::{CompatClient, CompatProtocol, SseFlow, envelope_error_message};
use super::gemini_compat::{self, GenerateContentResponse};
use super::{CompletionChunk, CompletionRequest, CompletionResponse};
use crate::debug_log;
use crate::error::AppResult;

/// `LLMProvider` over the Google Generative Language API.
pub type GeminiProvider = CompatClient<GeminiProtocol>;

pub struct GeminiProtocol;

impl CompatProtocol for GeminiProtocol {
    const LOG_TAG: &'static str = "gemini";

    fn completions_url(base_url: &str, model: &str, stream: bool) -> String {
        if stream {
            format!("{base_url}/models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("{base_url}/models/{model}:generateContent")
        }
    }

    fn apply_headers(
        request: reqwest::RequestBuilder,
        api_key: Option<&str>,
    ) -> reqwest::RequestBuilder {
        match api_key {
            Some(key) => request.header("x-goog-api-key", key),
            None => request,
        }
    }

    fn request_body(request: &CompletionRequest, _model: &str, _stream: bool) -> serde_json::Value {
        // Model and streaming mode live in the URL, not the body.
        gemini_compat::request_to_gemini(request)
    }

    fn error_message(body: &str) -> Option<String> {
        // Google's error envelope is {"error": {"message": ..., ...}}.
        envelope_error_message(body)
    }

    fn parse_response(body: &[u8]) -> AppResult<CompletionResponse> {
        let parsed: GenerateContentResponse = serde_json::from_slice(body)?;
        Ok(gemini_compat::response_from_gemini(parsed))
    }

    fn handle_sse_payload(payload: &str, emit: &mut dyn FnMut(CompletionChunk)) -> SseFlow {
        let Ok(chunk) = serde_json::from_str::<GenerateContentResponse>(payload) else {
            debug_log::log("gemini.stream.parse", "skipping malformed chunk");
            return SseFlow::Continue;
        };
        let (text, finish) = gemini_compat::extract_piece(&chunk);
        if !text.is_empty() {
            emit(CompletionChunk {
                content: text,
                tool_calls: Vec::new(),
                finish_reason: None,
            });
        }
        if let Some(reason) = finish {
            emit(CompletionChunk {
                content: String::new(),
                tool_calls: Vec::new(),
                finish_reason: Some(reason),
            });
            return SseFlow::Done;
        }
        SseFlow::Continue
    }

    fn parse_models(body: &[u8]) -> AppResult<Vec<String>> {
        let page: gemini_compat::ModelsPage = serde_json::from_slice(body)?;
        Ok(page
            .models
            .into_iter()
            .map(|model| model.name.trim_start_matches("models/").to_string())
            .collect())
    }
}
