//! OpenAI-compatible protocol plug-in (LM Studio, Ollama's `/v1`, vLLM,
//! OpenRouter, …) for the shared [`CompatClient`] transport. The wire
//! format lives in [`super::openai_compat`]; this file contributes only
//! the protocol details: endpoint/auth shape, error envelope, SSE payload
//! interpretation, model-list shape.

use super::compat_client::{CompatClient, CompatProtocol, SseFlow};
use super::openai_compat::{self, ChatCompletionChunk, ChatCompletionResponse};
use super::{CompletionChunk, CompletionRequest, CompletionResponse};
use crate::debug_log;
use crate::error::AppResult;

/// `LLMProvider` over any OpenAI-compatible endpoint.
pub type OpenAiCompatProvider = CompatClient<OpenAiProtocol>;

pub struct OpenAiProtocol;

impl CompatProtocol for OpenAiProtocol {
    const LOG_TAG: &'static str = "openai_compat";

    fn completions_url(base_url: &str, _model: &str, _stream: bool) -> String {
        format!("{base_url}/chat/completions")
    }

    fn apply_headers(
        request: reqwest::RequestBuilder,
        api_key: Option<&str>,
    ) -> reqwest::RequestBuilder {
        match api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    fn request_body(request: &CompletionRequest, model: &str, stream: bool) -> serde_json::Value {
        let mut body = openai_compat::request_to_openai(request);
        body["stream"] = serde_json::json!(stream);
        // The entry's model wins over whatever the internal request carries —
        // the TUI's model field is a DeepSeek-ism ("deepseek-chat"/"expert").
        body["model"] = serde_json::json!(model);
        body
    }

    fn error_message(body: &str) -> Option<String> {
        // Surface the OpenAI error envelope's message when there is one.
        serde_json::from_str::<openai_compat::ErrorResponse>(body)
            .ok()
            .map(|envelope| envelope.error.message)
    }

    fn parse_response(body: &[u8]) -> AppResult<CompletionResponse> {
        let parsed: ChatCompletionResponse = serde_json::from_slice(body)?;
        Ok(openai_compat::response_from_openai(parsed))
    }

    fn handle_sse_payload(payload: &str, emit: &mut dyn FnMut(CompletionChunk)) -> SseFlow {
        if payload == "[DONE]" {
            return SseFlow::Done;
        }
        match serde_json::from_str::<ChatCompletionChunk>(payload) {
            Ok(chunk) => emit(openai_compat::chunk_from_openai(chunk)),
            Err(error) => {
                // One malformed chunk shouldn't kill the stream, but it
                // must not vanish silently either.
                debug_log::log(
                    "openai_compat.stream.parse",
                    format!("skipping malformed chunk: {error}"),
                );
            }
        }
        SseFlow::Continue
    }

    fn parse_models(body: &[u8]) -> AppResult<Vec<String>> {
        let list: openai_compat::ModelList = serde_json::from_slice(body)?;
        Ok(list.data.into_iter().map(|model| model.id).collect())
    }
}
