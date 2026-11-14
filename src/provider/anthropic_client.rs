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

    type StreamState = anthropic_compat::AnthropicStreamState;

    fn handle_sse_payload(
        payload: &str,
        state: &mut Self::StreamState,
        emit: &mut dyn FnMut(CompletionChunk),
    ) -> SseFlow {
        if let Some(usage) = anthropic_compat::stream_usage(payload) {
            state.merge_usage(usage);
        }
        match anthropic_compat::parse_stream_event(payload) {
            StreamEvent::Text(text) => {
                emit(CompletionChunk {
                    content: text,
                    tool_calls: Vec::new(),
                    finish_reason: None,
                    usage: None,
                });
                SseFlow::Continue
            }
            StreamEvent::ToolStart { index, id, name } => {
                state.start_tool(index, id, name);
                heartbeat(emit)
            }
            StreamEvent::ToolDelta {
                index,
                partial_json,
            } => {
                state.append_tool_json(index, &partial_json);
                heartbeat(emit)
            }
            StreamEvent::ToolStop { index } => {
                state.finish_tool(index);
                heartbeat(emit)
            }
            StreamEvent::Finish(stop_reason) => {
                state.set_finish_reason(stop_reason);
                heartbeat(emit)
            }
            StreamEvent::Done => {
                // Терминальный чанк: причина остановки — из message_delta,
                // по умолчанию "stop", если сервер её не прислал.
                emit(CompletionChunk {
                    content: String::new(),
                    tool_calls: state.take_completed(),
                    finish_reason: Some(
                        state.finish_reason().unwrap_or_else(|| "stop".to_string()),
                    ),
                    usage: state.usage(),
                });
                SseFlow::Done
            }
            StreamEvent::Error(message) => SseFlow::Fail(message),
            // ping, thinking: поток жив, хоть текста и нет.
            StreamEvent::Ignore => heartbeat(emit),
        }
    }

    fn parse_models(body: &[u8]) -> AppResult<Vec<String>> {
        let page: anthropic_compat::ModelsPage = serde_json::from_slice(body)?;
        Ok(page.data.into_iter().map(|model| model.id).collect())
    }

    fn parse_limits(
        headers: &reqwest::header::HeaderMap,
        now: std::time::SystemTime,
    ) -> Option<crate::provider::failure::RateLimitHeaders> {
        Some(crate::provider::failure::parse_anthropic_limits(
            headers, now,
        ))
    }
}

/// Пустой кусок: поток жив. Большой родной `write` льётся аргументами дольше
/// сторожа простоя, и без этого здоровый поток убивался посередине.
fn heartbeat(emit: &mut dyn FnMut(CompletionChunk)) -> SseFlow {
    emit(CompletionChunk {
        content: String::new(),
        tool_calls: Vec::new(),
        finish_reason: None,
        usage: None,
    });
    SseFlow::Continue
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    /// Аргументы родного вызова льются минутами без текста — каждый кадр
    /// обязан держать сторож простоя.
    #[test]
    fn a_tool_argument_delta_still_shows_the_stream_is_alive() {
        let mut beats = 0;
        let flow = heartbeat(&mut |chunk: CompletionChunk| {
            assert!(chunk.content.is_empty() && chunk.finish_reason.is_none());
            beats += 1;
        });
        assert!(matches!(flow, SseFlow::Continue));
        assert_eq!(beats, 1);
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use crate::provider::compat_client::CompatProtocol;

    fn run(payloads: &[&str]) -> Vec<CompletionChunk> {
        let mut state = anthropic_compat::AnthropicStreamState::default();
        let mut out = Vec::new();
        for payload in payloads {
            AnthropicProtocol::handle_sse_payload(payload, &mut state, &mut |c| out.push(c));
        }
        out
    }

    /// Вход приходит в `message_start`, итог выхода — в `message_delta`; кэш входит во вход.
    #[test]
    fn usage_from_start_and_delta_lands_on_the_terminal_chunk() {
        let chunks = run(&[
            r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20,"output_tokens":1}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42,"output_tokens_details":{"thinking_tokens":7}}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let last = chunks.last().unwrap();
        assert_eq!(last.finish_reason.as_deref(), Some("stop"));
        let usage = last.usage.clone().expect("usage on the terminal chunk");
        assert_eq!(usage.prompt_tokens, 125);
        assert_eq!(usage.completion_tokens, 42);
        assert_eq!(usage.total_tokens, 167);
        assert_eq!(usage.cache_read_tokens, Some(100));
        assert_eq!(usage.cache_write_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    #[test]
    fn a_stream_without_usage_events_has_no_usage() {
        let chunks = run(&[
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(chunks.last().unwrap().usage.is_none());
    }

    #[test]
    fn a_blocking_response_counts_cache_in_the_prompt() {
        let body = br#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":10}}"#;
        let response = AnthropicProtocol::parse_response(body).unwrap();
        let usage = response.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.total_tokens), (13, 17));
    }

    #[test]
    fn a_blocking_response_without_usage_reports_none() {
        let body = br#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#;
        assert!(
            AnthropicProtocol::parse_response(body)
                .unwrap()
                .usage
                .is_none()
        );
    }
}
