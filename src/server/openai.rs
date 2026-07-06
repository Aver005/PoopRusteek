//! The OpenAI Chat Completions dialect of the API server:
//! `GET /v1/models` + `POST /v1/chat/completions` (the `/v1`-less spellings
//! work too). Wire types and conversions come from
//! `provider::openai_compat`; this file is routing + execution:
//! resolve the model id to a backend, run the completion on a fresh fork,
//! and (for streams) bridge `CompletionChunk`s into SSE frames ending with
//! the literal `data: [DONE]`.

use super::catalog::{self, ResolvedModel};
use super::http::{json_response, ApiBody, LogDetail, ServerContext};
use crate::provider::openai_compat::{
    self, ChatCompletionRequest, CompletionMeta, ErrorResponse,
};
use crate::provider::{CompletionRequest, LLMProvider};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{header, Method, Request, Response, StatusCode};
use serde::Serialize;
use std::sync::Arc;

/// Requests are buffered before parsing; cap them so a misbehaving client
/// can't balloon memory. 16 MiB fits any realistic chat history.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Dispatch one request. `None` = not a route of this dialect (the caller
/// answers 404).
pub(super) async fn route(
    request: Request<Incoming>,
    context: &Arc<ServerContext>,
) -> Option<Response<ApiBody>> {
    // Accept both the canonical `/v1/...` paths and bare ones — local
    // OpenAI-compatible servers commonly answer both.
    let path = request.uri().path().to_string();
    let path = path.strip_prefix("/v1").unwrap_or(&path).to_string();

    match (request.method().clone(), path.as_str()) {
        (Method::GET, "/models") => Some(models_response(context)),
        (Method::POST, "/chat/completions") => Some(chat_completions(request, context).await),
        _ => None,
    }
}

fn models_response(context: &ServerContext) -> Response<ApiBody> {
    let ids = catalog::list_model_ids(
        context.deepseek.is_some(),
        &context.entries,
        &context.models.snapshot(),
    );
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let created = chrono::Utc::now().timestamp().max(0) as u64;
    json_response(
        StatusCode::OK,
        &openai_compat::model_list(&ids, "pooprusteek", created),
    )
}

async fn chat_completions(
    request: Request<Incoming>,
    context: &Arc<ServerContext>,
) -> Response<ApiBody> {
    let body = match http_body_util::Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorResponse::invalid_request(format!(
                    "request body unreadable or larger than {MAX_BODY_BYTES} bytes"
                )),
            );
        }
    };

    let wire: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request(format!("invalid request body: {error}")),
            );
        }
    };

    if wire.tools.as_ref().is_some_and(|tools| !tools.is_null()) {
        // Captured, not silently swallowed (see openai_compat) — but v1
        // doesn't translate structured tool-calling, so the caller's tools
        // are ignored rather than rejected (many clients always send them).
        tracing::warn!("server: request carries `tools` — structured tool-calling is not translated, ignoring");
    }

    let resolved = match catalog::resolve_model(
        &wire.model,
        context.deepseek.is_some(),
        &context.entries,
        &context.models.snapshot(),
    ) {
        Ok(resolved) => resolved,
        Err(message) => {
            return json_response(
                StatusCode::NOT_FOUND,
                &ErrorResponse {
                    error: openai_compat::ErrorBody {
                        message,
                        kind: "invalid_request_error".to_string(),
                        code: Some("model_not_found".to_string()),
                    },
                },
            );
        }
    };

    // Stateless session strategy: every request runs on its own fresh
    // provider. DeepSeek gets a fork of the shared base (one rate limiter
    // for all server traffic); entries are built per request because a
    // caller-chosen sub-model lives in the entry config itself.
    let provider: Arc<dyn LLMProvider> = match &resolved {
        ResolvedModel::Deepseek { .. } => match &context.deepseek {
            Some(base) => base.fork(),
            None => unreachable!("resolve_model only yields Deepseek when the backend exists"),
        },
        ResolvedModel::Entry { entry } => match crate::provider::build_entry_provider(entry) {
            Ok(provider) => provider,
            Err(error) => {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorResponse::server_error(format!(
                        "failed to initialize provider '{}': {error}",
                        entry.name
                    )),
                );
            }
        },
    };

    // The response echoes the id the caller asked for, per OpenAI protocol.
    let public_model = wire.model.clone();
    let mut internal = match openai_compat::to_internal_request(wire, &context.defaults) {
        Ok(internal) => internal,
        Err(message) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request(message),
            );
        }
    };
    internal.model = resolved.internal_model().to_string();

    let meta = CompletionMeta::generate(&public_model);
    let streaming = internal.stream;
    let mut response = if streaming {
        stream_completion(provider, resolved.is_deepseek(), internal, meta)
    } else {
        blocking_completion(provider, resolved.is_deepseek(), internal, meta).await
    };
    // Enriches the proxy access log; free otherwise.
    response.extensions_mut().insert(LogDetail(format!(
        "model={public_model} → {}{}",
        resolved.internal_model(),
        if streaming { " (stream)" } else { "" }
    )));
    response
}

async fn blocking_completion(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    let prompt_texts: Vec<String> = request
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    let result = provider.complete(request).await;
    discard_deepseek_session(is_deepseek, &provider).await;
    match result {
        Ok(response) => {
            let prompt_refs: Vec<&str> = prompt_texts.iter().map(String::as_str).collect();
            let fallback = openai_compat::estimated_usage(&prompt_refs, &response.content);
            json_response(
                StatusCode::OK,
                &openai_compat::response_to_openai(&response, fallback, &meta),
            )
        }
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorResponse::server_error(error.to_string()),
        ),
    }
}

/// Streaming path: respond immediately with an SSE body fed by a bridge
/// task. Errors after the 200 has been committed travel in-stream as an
/// `{"error": …}` data line — the same convention OpenAI-compatible
/// proxies use.
fn stream_completion(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    let (body_tx, mut body_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(bridge_stream(provider, is_deepseek, request, meta, body_tx));

    let stream = futures::stream::poll_fn(move |task_context| {
        body_rx
            .poll_recv(task_context)
            .map(|next| next.map(|bytes| Ok(Frame::data(bytes))))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(StreamBody::new(stream).boxed())
        .expect("static response parts are valid")
}

/// Pump provider chunks into SSE frames. Runs detached from the HTTP
/// response; a dropped `body_tx` receiver (client hung up) aborts the
/// upstream call instead of streaming into the void.
async fn bridge_stream(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    meta: CompletionMeta,
    body_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
) {
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
    let upstream = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.complete_stream(request, chunk_tx).await })
    };

    let mut first = true;
    let mut finish_reason: Option<String> = None;
    let mut client_gone = false;
    while let Some(chunk) = chunk_rx.recv().await {
        if chunk.finish_reason.is_some() {
            finish_reason = chunk.finish_reason.clone();
        }
        if chunk.content.is_empty() {
            continue;
        }
        let frame = sse_data(&openai_compat::delta_chunk(&chunk.content, first, &meta));
        first = false;
        if body_tx.send(frame).is_err() {
            client_gone = true;
            upstream.abort();
            break;
        }
    }

    if !client_gone {
        match upstream.await {
            Ok(Ok(())) => {
                let reason = finish_reason.as_deref().unwrap_or("stop");
                let _ = body_tx.send(sse_data(&openai_compat::final_chunk(reason, &meta)));
                let _ = body_tx.send(Bytes::from_static(b"data: [DONE]\n\n"));
            }
            Ok(Err(error)) => {
                let _ = body_tx.send(sse_data(&ErrorResponse::server_error(error.to_string())));
            }
            Err(join_error) => {
                let _ = body_tx.send(sse_data(&ErrorResponse::server_error(format!(
                    "completion task failed: {join_error}"
                ))));
            }
        }
    }

    discard_deepseek_session(is_deepseek, &provider).await;
}

/// One SSE `data:` frame.
fn sse_data(payload: &impl Serialize) -> Bytes {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {json}\n\n"))
}

/// Server completions are one-shot: a DeepSeek fork leaves a remote chat
/// session behind, and nothing will ever continue it — delete it so API
/// traffic doesn't pile junk chats onto the user's account. Best-effort.
async fn discard_deepseek_session(is_deepseek: bool, provider: &Arc<dyn LLMProvider>) {
    if !is_deepseek {
        return;
    }
    if let Err(error) = provider.discard_remote_session().await {
        tracing::debug!("server: failed to discard ephemeral DeepSeek session: {error}");
    }
}

#[cfg(test)]
mod tests {
    use crate::app::events::AppEvent;
    use crate::config::{ProviderEntry, ProviderProtocol, ServerApi};
    use crate::provider::openai_compat::RequestDefaults;
    use crate::server::{spawn, ServerSettings};
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;

    /// A one-endpoint OpenAI-compatible upstream: canned non-stream JSON or
    /// a canned SSE stream, picked by the request's `stream` flag. Loopback
    /// only — lets the whole gateway chain run without leaving the machine.
    async fn spawn_mock_upstream() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let addr = listener.local_addr().expect("mock upstream addr");
        tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(answer_mock_request);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        addr
    }

    async fn answer_mock_request(
        request: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
        let body = request
            .into_body()
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let streaming = parsed["stream"].as_bool().unwrap_or(false);
        let response = if streaming {
            let sse = concat!(
                "data: {\"id\":\"up-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"up-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from_static(sse.as_bytes())))
        } else {
            // Echo the model the gateway sent so the test can verify the
            // caller-chosen sub-model actually reached the upstream.
            let payload = serde_json::json!({
                "id": "up-1",
                "object": "chat.completion",
                "created": 1,
                "model": parsed["model"],
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": format!("Hello from {}", parsed["model"].as_str().unwrap_or("?"))},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            });
            Response::builder()
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(payload.to_string())))
        };
        Ok(response.expect("static response parts are valid"))
    }

    fn gateway_settings(upstream: std::net::SocketAddr) -> ServerSettings {
        ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 0,
            api: ServerApi::Openai,
            api_key: None,
            defaults: RequestDefaults { temperature: 0.7, max_tokens: 256 },
            deepseek: None,
            entries: vec![ProviderEntry {
                name: "mock".to_string(),
                base_url: format!("http://{upstream}"),
                api_key: None,
                model: "m".to_string(),
                protocol: ProviderProtocol::Openai,
            }],
            request_log: false,
        }
    }

    /// Full gateway round trip over real sockets: OpenAI request in →
    /// entry resolution → upstream OpenAI call → OpenAI response out, in
    /// both the blocking and the SSE-streaming shape.
    #[tokio::test]
    async fn gateway_completes_against_an_entry_backend() {
        let upstream = spawn_mock_upstream().await;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
        let handle = spawn(gateway_settings(upstream), models, 1, event_tx);
        let addr = match event_rx.recv().await {
            Some(AppEvent::ServerStarted { addr, .. }) => addr,
            _ => panic!("expected ServerStarted"),
        };
        let client = reqwest::Client::new();

        // The catalog advertises the entry as `mock/m`.
        let models: serde_json::Value = client
            .get(format!("http://{addr}/v1/models"))
            .send()
            .await
            .expect("models request")
            .json()
            .await
            .expect("models json");
        assert_eq!(models["data"][0]["id"], "mock/m");

        // Non-streaming, with a caller-chosen sub-model: `mock/custom`
        // must reach the upstream as model "custom".
        let completion: serde_json::Value = client
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "mock/custom",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .send()
            .await
            .expect("completion request")
            .json()
            .await
            .expect("completion json");
        assert_eq!(completion["object"], "chat.completion");
        // The response echoes the id the caller asked for...
        assert_eq!(completion["model"], "mock/custom");
        // ...while the upstream saw the sub-model half.
        assert_eq!(
            completion["choices"][0]["message"]["content"],
            "Hello from custom"
        );
        // Real upstream usage rides through untouched.
        assert_eq!(completion["usage"]["total_tokens"], 5);

        // Streaming: deltas re-framed under one completion id, terminal
        // chunk carries the finish_reason, then the literal [DONE].
        let stream_body = client
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "mock",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true,
            }))
            .send()
            .await
            .expect("stream request")
            .text()
            .await
            .expect("stream body");
        let payloads: Vec<&str> = stream_body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect();
        assert_eq!(payloads.last(), Some(&"[DONE]"));
        let first: serde_json::Value =
            serde_json::from_str(payloads[0]).expect("first chunk json");
        assert_eq!(first["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(first["choices"][0]["delta"]["content"], "Hel");
        let contents: String = payloads[..payloads.len() - 1]
            .iter()
            .map(|payload| serde_json::from_str::<serde_json::Value>(payload).expect("chunk json"))
            .filter_map(|chunk| {
                chunk["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(contents, "Hello");
        let last: serde_json::Value = serde_json::from_str(payloads[payloads.len() - 2])
            .expect("terminal chunk json");
        assert_eq!(last["choices"][0]["finish_reason"], "stop");

        handle.request_shutdown();
    }
}
