//! Anthropic-compatible protocol plug-in (the Anthropic API itself,
//! Claude-compatible proxies/gateways) for the shared [`CompatClient`]
//! transport. The wire format lives in [`super::anthropic_compat`]; this
//! file contributes only the protocol details.
//!
//! Auth follows the Anthropic convention: the key goes in `x-api-key`
//! (not a bearer header) alongside a pinned `anthropic-version`.

use super::anthropic_compat::{self, StreamEvent};
use super::compat_client::{CompatClient, CompatProtocol, SseFlow, envelope_error_message};
use super::{CompletionChunk, CompletionRequest, CompletionResponse};
use crate::error::AppResult;

/// The Messages API requires this header; compat servers accept it.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `LLMProvider` over any Anthropic-compatible endpoint.
pub type AnthropicCompatProvider = CompatClient<AnthropicProtocol>;

pub struct AnthropicProtocol;

impl CompatProtocol for AnthropicProtocol {
    const LOG_TAG: &'static str = "anthropic_compat";

    fn completions_url(base_url: &str, _model: &str, _stream: bool) -> String {
        format!("{base_url}/messages")
    }

    fn apply_headers(
        request: reqwest::RequestBuilder,
        api_key: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = request.header("anthropic-version", ANTHROPIC_VERSION);
        match api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    fn request_body(request: &CompletionRequest, model: &str, stream: bool) -> serde_json::Value {
        let mut body = anthropic_compat::request_to_anthropic(request);
        body["stream"] = serde_json::json!(stream);
        body["model"] = serde_json::json!(model);
        // Modern Claude models (Opus 4.7+, Sonnet 5, Fable/Mythos 5) return
        // 400 on any sampling parameter — drop it for those families only.
        if anthropic_compat::model_rejects_sampling(model)
            && let Some(object) = body.as_object_mut()
        {
            object.remove("temperature");
        }
        body
    }

    fn error_message(body: &str) -> Option<String> {
        // Anthropic's error envelope is {"type":"error","error":{...}}.
        envelope_error_message(body)
    }

    fn parse_response(body: &[u8]) -> AppResult<CompletionResponse> {
        let parsed: anthropic_compat::MessagesResponse = serde_json::from_slice(body)?;
        Ok(anthropic_compat::response_from_anthropic(parsed))
    }

    fn handle_sse_payload(payload: &str, emit: &mut dyn FnMut(CompletionChunk)) -> SseFlow {
        match anthropic_compat::parse_stream_event(payload) {
            StreamEvent::Text(text) => {
                emit(CompletionChunk {
                    content: text,
                    finish_reason: None,
                });
                SseFlow::Continue
            }
            StreamEvent::Done => {
                emit(CompletionChunk {
                    content: String::new(),
                    finish_reason: Some("stop".to_string()),
                });
                SseFlow::Done
            }
            StreamEvent::Error(message) => SseFlow::Fail(message),
            StreamEvent::Ignore => SseFlow::Continue,
        }
    }

    fn parse_models(body: &[u8]) -> AppResult<Vec<String>> {
        let page: anthropic_compat::ModelsPage = serde_json::from_slice(body)?;
        Ok(page.data.into_iter().map(|model| model.id).collect())
    }
}
