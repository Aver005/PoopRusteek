//! OpenAI-compatible protocol plug-in (LM Studio, Ollama's `/v1`, vLLM,
//! OpenRouter, …) for the shared [`CompatClient`] transport. The wire
//! format lives in [`super::openai_compat`]; this file contributes only
//! the protocol details: endpoint/auth shape, error envelope, SSE payload
//! interpretation, model-list shape.

use super::compat_client::{CompatClient, CompatProtocol, SseFlow};
use super::openai_compat::{
    self, ChatCompletionChunk, ChatCompletionResponse, WireFunction, WireToolCall,
};
use super::{CompletionChunk, CompletionRequest, CompletionResponse, ToolCall};
use crate::debug_log;
use crate::error::AppResult;

/// Один вызов инструмента, собираемый из дельт стрима по его позиции
/// (`index`, либо "следующая по счёту", если сервер его не прислал).
#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    /// Куски `arguments`, дописываемые по мере прихода дельт.
    arguments: String,
}

/// Состояние сборки вызовов инструментов одного стрима OpenAI-совместимого
/// протокола: фрагменты `delta.tool_calls[]` приходят по кускам, наверх
/// вызовы поднимаются только целиком.
#[derive(Default)]
pub struct OpenAiStreamState {
    pending: Vec<PendingToolCall>,
    /// Уже был отдан терминальный чанк с `finish_reason` — `[DONE]` после
    /// него не должен слать вызовы повторно.
    finished: bool,
}

impl OpenAiStreamState {
    /// Вливает один фрагмент дельты в накопленный вызов на его позиции.
    fn merge(&mut self, fragment: &WireToolCall) {
        // Некоторые серверы (Ollama и другие) не шлют `index`, когда весь
        // вызов приходит одной дельтой — тогда это "следующая" позиция.
        let position = fragment
            .index
            .map(|i| i as usize)
            .unwrap_or(self.pending.len());
        if self.pending.len() <= position {
            self.pending
                .resize_with(position + 1, PendingToolCall::default);
        }
        let slot = &mut self.pending[position];
        if !fragment.id.is_empty() {
            slot.id = Some(fragment.id.clone());
        }
        if !fragment.function.name.is_empty() {
            slot.name = Some(fragment.function.name.clone());
        }
        slot.arguments.push_str(&fragment.function.arguments);
    }

    /// Собранные вызовы (аргументы разобраны как JSON или сохранены строкой).
    /// Без id сервер бывает — свой придумываем; без имени вызов не исполнить.
    fn assembled(&self) -> Vec<ToolCall> {
        self.pending
            .iter()
            .filter_map(|call| {
                let Some(name) = call.name.clone() else {
                    debug_log::log(
                        "openai_compat.tool_call.nameless",
                        format!("dropping a call without a name: {}", call.arguments),
                    );
                    return None;
                };
                let id = call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
                let wire = WireToolCall {
                    index: None,
                    id,
                    kind: "function".to_string(),
                    function: WireFunction {
                        name,
                        arguments: call.arguments.clone(),
                    },
                };
                Some(wire.to_internal())
            })
            .collect()
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

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

    type StreamState = OpenAiStreamState;

    fn handle_sse_payload(
        payload: &str,
        state: &mut Self::StreamState,
        emit: &mut dyn FnMut(CompletionChunk),
    ) -> SseFlow {
        if payload == "[DONE]" {
            // Сервер закрыл поток, не прислав чанк с `finish_reason` — если
            // вызовы всё же собрались, поднимаем их сейчас, иначе теряются.
            if !state.finished && state.has_pending() {
                emit(CompletionChunk {
                    content: String::new(),
                    tool_calls: state.assembled(),
                    finish_reason: Some("tool_calls".to_string()),
                });
            }
            return SseFlow::Done;
        }
        let chunk: ChatCompletionChunk = match serde_json::from_str(payload) {
            Ok(chunk) => chunk,
            Err(error) => {
                // One malformed chunk shouldn't kill the stream, but it
                // must not vanish silently either.
                debug_log::log(
                    "openai_compat.stream.parse",
                    format!("skipping malformed chunk: {error}"),
                );
                return SseFlow::Continue;
            }
        };
        let Some(choice) = chunk.choices.into_iter().next() else {
            return SseFlow::Continue;
        };
        for fragment in choice.delta.tool_calls.iter().flatten() {
            state.merge(fragment);
        }
        match choice.finish_reason {
            Some(finish_reason) => {
                state.finished = true;
                emit(CompletionChunk {
                    content: choice.delta.content.unwrap_or_default(),
                    tool_calls: state.assembled(),
                    finish_reason: Some(finish_reason),
                });
            }
            None => {
                // Текстовые дельты поднимаются как раньше — по одной на
                // payload, даже пустые (роль-объявляющий первый чанк).
                emit(CompletionChunk {
                    content: choice.delta.content.unwrap_or_default(),
                    tool_calls: Vec::new(),
                    finish_reason: None,
                });
            }
        }
        SseFlow::Continue
    }

    fn parse_models(body: &[u8]) -> AppResult<Vec<String>> {
        let list: openai_compat::ModelList = serde_json::from_slice(body)?;
        Ok(list.data.into_iter().map(|model| model.id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Прогоняет payload'ы через `handle_sse_payload` с общим состоянием и
    /// собирает все чанки, поднятые через `emit`.
    fn run_payloads(payloads: &[String]) -> Vec<CompletionChunk> {
        let mut state = OpenAiStreamState::default();
        let mut emitted = Vec::new();
        for payload in payloads {
            OpenAiProtocol::handle_sse_payload(payload, &mut state, &mut |chunk| {
                emitted.push(chunk);
            });
        }
        emitted
    }

    fn delta_chunk_payload(delta_json: serde_json::Value, finish_reason: Option<&str>) -> String {
        serde_json::json!({
            "id": "chatcmpl-x",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "m",
            "choices": [{"index": 0, "delta": delta_json, "finish_reason": finish_reason}],
        })
        .to_string()
    }

    /// Аргументы одного вызова, растянутые на 3 дельты — наверх поднимаются
    /// только целиком, в терминальном чанке.
    #[test]
    fn arguments_split_across_three_deltas_assemble_whole() {
        let payloads = [
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":"}}]}),
                None,
            ),
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Bost"}}]}),
                None,
            ),
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "function": {"arguments": "on\"}"}}]}),
                None,
            ),
            delta_chunk_payload(serde_json::json!({}), Some("tool_calls")),
        ];
        let chunks = run_payloads(&payloads);
        let last = chunks.last().unwrap();
        assert_eq!(last.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(last.tool_calls.len(), 1);
        assert_eq!(last.tool_calls[0].id, "call_1");
        assert_eq!(last.tool_calls[0].name, "get_weather");
        assert_eq!(
            last.tool_calls[0].arguments,
            serde_json::json!({"city": "Boston"})
        );
    }

    /// Два параллельных вызова, чьи дельты приходят вперемешку по `index`.
    #[test]
    fn two_parallel_calls_interleaved_by_index_assemble_separately() {
        let payloads = [
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1"}}]}),
                None,
            ),
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 1, "id": "call_b", "type": "function", "function": {"name": "b", "arguments": "{\"y\":2"}}]}),
                None,
            ),
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "function": {"arguments": "}"}}]}),
                None,
            ),
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 1, "function": {"arguments": "}"}}]}),
                None,
            ),
            delta_chunk_payload(serde_json::json!({}), Some("tool_calls")),
        ];
        let chunks = run_payloads(&payloads);
        let last = chunks.last().unwrap();
        assert_eq!(last.tool_calls.len(), 2);
        assert_eq!(last.tool_calls[0].id, "call_a");
        assert_eq!(last.tool_calls[0].arguments, serde_json::json!({"x": 1}));
        assert_eq!(last.tool_calls[1].id, "call_b");
        assert_eq!(last.tool_calls[1].arguments, serde_json::json!({"y": 2}));
    }

    /// Сервер (например Ollama) шлёт весь вызов одной дельтой без `index`.
    #[test]
    fn whole_call_in_one_delta_without_index() {
        let payloads = [
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "ping", "arguments": "{}"}}]}),
                None,
            ),
            delta_chunk_payload(serde_json::json!({}), Some("tool_calls")),
        ];
        let chunks = run_payloads(&payloads);
        let last = chunks.last().unwrap();
        assert_eq!(last.tool_calls.len(), 1);
        assert_eq!(last.tool_calls[0].id, "call_1");
        assert_eq!(last.tool_calls[0].name, "ping");
        assert_eq!(last.tool_calls[0].arguments, serde_json::json!({}));
    }

    /// Сервер без id вызова: id придумывается, вызов не теряется. Вызов без
    /// имени исполнить нельзя — он отбрасывается.
    #[test]
    fn a_call_without_an_id_gets_one_and_a_nameless_call_is_dropped() {
        let payloads = [
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [
                    {"index": 0, "function": {"name": "ping", "arguments": "{}"}},
                    {"index": 1, "function": {"arguments": "{}"}}
                ]}),
                None,
            ),
            delta_chunk_payload(serde_json::json!({}), Some("tool_calls")),
        ];
        let chunks = run_payloads(&payloads);
        let calls = &chunks.last().unwrap().tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ping");
        assert!(calls[0].id.starts_with("call_"), "{}", calls[0].id);
    }

    /// `[DONE]` приходит без чанка с `finish_reason` — собранные вызовы
    /// поднимаются здесь, а не теряются молча.
    #[test]
    fn done_without_prior_finish_reason_flushes_pending_calls() {
        let mut state = OpenAiStreamState::default();
        let mut emitted = Vec::new();
        let payload = delta_chunk_payload(
            serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "ping", "arguments": "{}"}}]}),
            None,
        );
        OpenAiProtocol::handle_sse_payload(&payload, &mut state, &mut |chunk| emitted.push(chunk));
        let flow = OpenAiProtocol::handle_sse_payload("[DONE]", &mut state, &mut |chunk| {
            emitted.push(chunk)
        });
        assert!(matches!(flow, SseFlow::Done));
        let last = emitted.last().unwrap();
        assert_eq!(last.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(last.tool_calls.len(), 1);
        assert_eq!(last.tool_calls[0].id, "call_1");
    }

    /// Аргументы, которые после склейки не парсятся как JSON — сохраняются
    /// строкой, а не теряются и не роняют стрим.
    #[test]
    fn invalid_json_arguments_are_kept_as_string() {
        let payloads = [
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "weird", "arguments": "not json"}}]}),
                None,
            ),
            delta_chunk_payload(serde_json::json!({}), Some("tool_calls")),
        ];
        let chunks = run_payloads(&payloads);
        let last = chunks.last().unwrap();
        assert_eq!(
            last.tool_calls[0].arguments,
            serde_json::Value::String("not json".to_string())
        );
    }

    /// Пустая строка аргументов (вызов вовсе без параметров) читается как `{}`.
    #[test]
    fn empty_arguments_become_empty_object() {
        let payloads = [
            delta_chunk_payload(
                serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "ping", "arguments": ""}}]}),
                None,
            ),
            delta_chunk_payload(serde_json::json!({}), Some("tool_calls")),
        ];
        let chunks = run_payloads(&payloads);
        let last = chunks.last().unwrap();
        assert_eq!(last.tool_calls[0].arguments, serde_json::json!({}));
    }
}
