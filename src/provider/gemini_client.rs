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

/// Вызовы инструментов, собранные по ходу стрима — отдаются одним куском
/// вместе с финальным чанком, а не по частям.
#[derive(Default)]
pub struct GeminiStreamState {
    tool_calls: Vec<crate::provider::ToolCall>,
}

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

    /// Копим `functionCall`-и по мере чтения стрима — наверх уходят только
    /// вместе с финальным чанком (см. доккомментарий модуля).
    type StreamState = GeminiStreamState;

    fn handle_sse_payload(
        payload: &str,
        state: &mut Self::StreamState,
        emit: &mut dyn FnMut(CompletionChunk),
    ) -> SseFlow {
        let Ok(chunk) = serde_json::from_str::<GenerateContentResponse>(payload) else {
            debug_log::log("gemini.stream.parse", "skipping malformed chunk");
            return SseFlow::Continue;
        };
        let (text, calls, finish) = gemini_compat::extract_piece(&chunk);
        state.tool_calls.extend(calls);
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
                tool_calls: std::mem::take(&mut state.tool_calls),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_payload(
        payload: &str,
        state: &mut GeminiStreamState,
    ) -> (Vec<CompletionChunk>, SseFlow) {
        let mut chunks = Vec::new();
        let flow =
            GeminiProtocol::handle_sse_payload(payload, state, &mut |chunk| chunks.push(chunk));
        (chunks, flow)
    }

    /// A text chunk, then a chunk carrying a `functionCall` part alongside
    /// `finishReason: STOP` — the call must ride out with the terminal
    /// chunk, mapped to the `tool_calls` finish reason.
    #[test]
    fn text_then_function_call_with_stop_emits_tool_calls() {
        let mut state = GeminiStreamState::default();

        let (chunks, flow) = run_payload(
            r#"{"candidates": [{"content": {"parts": [{"text": "Checking…"}]}}]}"#,
            &mut state,
        );
        assert!(matches!(flow, SseFlow::Continue));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "Checking…");
        assert!(chunks[0].tool_calls.is_empty());
        assert!(chunks[0].finish_reason.is_none());

        let (chunks, flow) = run_payload(
            r#"{
                "candidates": [{
                    "content": {"parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Boston"}}}]},
                    "finishReason": "STOP"
                }]
            }"#,
            &mut state,
        );
        assert!(matches!(flow, SseFlow::Done));
        assert_eq!(chunks.len(), 1, "text empty, finish carries only the call");
        let terminal = &chunks[0];
        assert_eq!(terminal.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(terminal.tool_calls.len(), 1);
        assert_eq!(terminal.tool_calls[0].name, "get_weather");
        assert_eq!(terminal.tool_calls[0].arguments["city"], "Boston");
        assert!(
            !terminal.tool_calls[0].id.is_empty(),
            "an id is synthesized even though Gemini sent none"
        );
    }

    /// `functionCall` missing `args` entirely → arguments default to `{}`.
    #[test]
    fn function_call_without_args_defaults_to_empty_object() {
        let mut state = GeminiStreamState::default();
        let (chunks, _) = run_payload(
            r#"{
                "candidates": [{
                    "content": {"parts": [{"functionCall": {"name": "ping"}}]},
                    "finishReason": "STOP"
                }]
            }"#,
            &mut state,
        );
        assert_eq!(chunks[0].tool_calls[0].arguments, serde_json::json!({}));
    }

    /// An `id` on the wire (some models do send one) wins over the
    /// synthesized uuid.
    #[test]
    fn function_call_id_from_the_wire_is_preferred() {
        let mut state = GeminiStreamState::default();
        let (chunks, _) = run_payload(
            r#"{
                "candidates": [{
                    "content": {"parts": [{"functionCall": {"id": "call_abc", "name": "ping", "args": {}}}]},
                    "finishReason": "STOP"
                }]
            }"#,
            &mut state,
        );
        assert_eq!(chunks[0].tool_calls[0].id, "call_abc");
    }

    /// `MAX_TOKENS` maps to `length` regardless of any collected calls.
    #[test]
    fn max_tokens_maps_to_length() {
        let mut state = GeminiStreamState::default();
        let (chunks, flow) = run_payload(
            r#"{"candidates": [{"finishReason": "MAX_TOKENS"}]}"#,
            &mut state,
        );
        assert!(matches!(flow, SseFlow::Done));
        assert_eq!(chunks[0].finish_reason.as_deref(), Some("length"));
    }
}
