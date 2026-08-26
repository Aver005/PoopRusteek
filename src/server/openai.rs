//! The OpenAI Chat Completions dialect of the API server:
//! `GET /v1/models` + `POST /v1/chat/completions` (the `/v1`-less spellings
//! work too). Wire types and conversions come from
//! `provider::openai_compat`; this file is routing + execution:
//! resolve the model id to a backend, run the completion on a fresh fork,
//! and (for streams) bridge `CompletionChunk`s into SSE frames ending with
//! the literal `data: [DONE]`.

use super::catalog::{self, ResolvedModel};
use super::http::{ApiBody, LogDetail, ServerContext, full_body, json_response};
use crate::config::ProviderProtocol;
use crate::provider::openai_compat::{
    self, ChatCompletionRequest, CompletionMeta, ErrorResponse, ReasoningStreamSplitter,
};
use crate::provider::{ChatMessage, CompletionRequest, CompletionResponse, LLMProvider};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Requests are buffered before parsing; cap them so a misbehaving client
/// can't balloon memory. 16 MiB fits any realistic chat history.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// A resolved backend, or a ready-to-return error response (boxed — see
/// [`resolve_backend`]).
type BackendResult = Result<(ResolvedModel, Arc<dyn LLMProvider>), Box<Response<ApiBody>>>;

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
        // Legacy text-completion endpoint (deprecated by OpenAI, still used
        // by some older clients) — synthesized from the chat path.
        (Method::POST, "/completions") => Some(legacy_completions(request, context).await),
        // Not part of chat: forwarded verbatim to the entry that serves the
        // requested model (embeddings/reranking are upstream-specific).
        (Method::POST, "/embeddings") => Some(passthrough(request, context, "/embeddings").await),
        (Method::POST, "/rerank") => Some(passthrough(request, context, "/rerank").await),
        _ => None,
    }
}

/// Read and size-cap a request body, or the ready-to-return error response.
async fn read_body(request: Request<Incoming>) -> Result<Bytes, Response<ApiBody>> {
    http_body_util::Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|_| {
            json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorResponse::invalid_request(format!(
                    "request body unreadable or larger than {MAX_BODY_BYTES} bytes"
                )),
            )
        })
}

fn model_not_found(message: String) -> Response<ApiBody> {
    json_response(
        StatusCode::NOT_FOUND,
        &ErrorResponse {
            error: openai_compat::ErrorBody {
                message,
                kind: "invalid_request_error".to_string(),
                code: Some("model_not_found".to_string()),
            },
        },
    )
}

/// Resolve a model id to a backend and build its provider, or the
/// ready-to-return error response (boxed — a `Response` dwarfs the Ok
/// tuple, and clippy rightly flags a fat `Err`). Shared by chat + legacy.
fn resolve_backend(model_id: &str, context: &ServerContext) -> BackendResult {
    let resolved = catalog::resolve_model(
        model_id,
        context.deepseek.is_some(),
        &context.entries,
        &context.models.snapshot(),
    )
    .map_err(|message| Box::new(model_not_found(message)))?;

    let provider: Arc<dyn LLMProvider> = match &resolved {
        ResolvedModel::Deepseek { .. } => context
            .deepseek
            .clone()
            .expect("resolve_model only yields Deepseek when the backend exists")
            .fork(),
        ResolvedModel::Entry { entry } => {
            crate::provider::build_entry_provider(entry).map_err(|error| {
                Box::new(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorResponse::server_error(format!(
                        "failed to initialize provider '{}': {error}",
                        entry.name
                    )),
                ))
            })?
        }
    };
    Ok((resolved, provider))
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
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
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
        tracing::warn!(
            "server: request carries `tools` — structured tool-calling is not translated, ignoring"
        );
    }

    let (resolved, provider) = match resolve_backend(&wire.model, context) {
        Ok(pair) => pair,
        Err(response) => return *response,
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
    let response = match run_blocking_completion(&provider, is_deepseek, request).await {
        Ok(response) => response,
        Err(error_response) => return error_response,
    };
    let prompt_refs: Vec<&str> = prompt_texts.iter().map(String::as_str).collect();
    let fallback = openai_compat::estimated_usage(&prompt_refs, &response.content);
    json_response(
        StatusCode::OK,
        &openai_compat::response_to_openai(&response, fallback, &meta),
    )
}

/// Run a blocking completion and discard the per-request DeepSeek session;
/// errors come back as a ready-to-return 500. Success-payload shaping is
/// each dialect's own business — this is the half `blocking_completion`
/// and `legacy_blocking` used to duplicate.
async fn run_blocking_completion(
    provider: &Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
) -> Result<CompletionResponse, Response<ApiBody>> {
    let result = provider.complete(request).await;
    discard_deepseek_session(is_deepseek, provider).await;
    result.map_err(|error| {
        json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorResponse::server_error(error.to_string()),
        )
    })
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
    sse_stream_response(move |body_tx| bridge_stream(provider, is_deepseek, request, meta, body_tx))
}

/// Respond immediately with an SSE body fed by a detached bridge task —
/// the channel/poll_fn/header scaffolding both dialects used to duplicate.
fn sse_stream_response<F, Fut>(bridge: F) -> Response<ApiBody>
where
    F: FnOnce(tokio::sync::mpsc::UnboundedSender<Bytes>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (body_tx, mut body_rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(bridge(body_tx));
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

/// Chat-dialect frame shaping over [`drive_sse_bridge`]. Routes `<think>`
/// reasoning into `reasoning_content` deltas live, so a reasoning model's
/// chain-of-thought reaches clients in the right field instead of
/// polluting the answer (see openai_compat::split_reasoning).
async fn bridge_stream(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    meta: CompletionMeta,
    body_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
) {
    drive_sse_bridge(
        provider,
        is_deepseek,
        request,
        body_tx,
        |reasoning, content, is_first| {
            if reasoning.is_empty() && content.is_empty() {
                return None; // held back a partial tag — nothing to emit yet
            }
            Some(sse_data(&openai_compat::split_delta_chunk(
                reasoning, content, is_first, &meta,
            )))
        },
        // Flush any tail the splitter held (e.g. an unterminated `<think>`)
        // as one more delta — even an errored stream delivers held-back text.
        |reasoning, content, is_first| {
            (!reasoning.is_empty() || !content.is_empty()).then(|| {
                sse_data(&openai_compat::split_delta_chunk(
                    reasoning, content, is_first, &meta,
                ))
            })
        },
        |_content, reason| sse_data(&openai_compat::final_chunk(reason, &meta)),
    )
    .await;
}

/// Idle backstop for the SSE bridges: provider-side read timeouts are a
/// convention (every current provider configures 120s), not a guarantee —
/// a silently stalled upstream must not pin a bridge task (and its client
/// connection) forever. Mirrors the agent loop's idle guard in
/// `agent/stream.rs`.
const UPSTREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Everything both SSE bridges share: spawn the upstream completion into a
/// chunk channel, pump chunks through the reasoning splitter, and handle
/// client hang-up, the idle backstop, the upstream post-mortem, and the
/// trailing `data: [DONE]`; the per-request DeepSeek session is discarded
/// on every exit path. The dialects contribute only frame shaping:
/// `delta_frame` renders one split delta (`None` = nothing to emit yet),
/// `tail_frame` renders the splitter's flushed remainder (chat emits it as
/// a delta, legacy returns `None` and folds it into its final frame), and
/// `final_frame` renders the success terminator. `is_first` stays true
/// until a delta has been sent — the chat dialect's first frame announces
/// the assistant role.
async fn drive_sse_bridge(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    body_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    mut delta_frame: impl FnMut(&str, &str, bool) -> Option<Bytes> + Send,
    tail_frame: impl FnOnce(&str, &str, bool) -> Option<Bytes> + Send,
    final_frame: impl FnOnce(&str, &str) -> Bytes + Send,
) {
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
    let upstream = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.complete_stream(request, chunk_tx).await })
    };

    let mut splitter = ReasoningStreamSplitter::new();
    let mut finish_reason: Option<String> = None;
    let mut sent_any = false;
    let mut aborted = false;
    loop {
        let chunk = match tokio::time::timeout(UPSTREAM_IDLE_TIMEOUT, chunk_rx.recv()).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break, // upstream closed its sender — post-mortem below
            Err(_elapsed) => {
                upstream.abort();
                let _ = body_tx.send(sse_data(&ErrorResponse::server_error(format!(
                    "upstream produced no data for {}s; stream aborted",
                    UPSTREAM_IDLE_TIMEOUT.as_secs()
                ))));
                aborted = true;
                break;
            }
        };
        if chunk.finish_reason.is_some() {
            finish_reason = chunk.finish_reason.clone();
        }
        if chunk.content.is_empty() {
            continue;
        }
        let (reasoning, content) = splitter.push(&chunk.content);
        let Some(frame) = delta_frame(&reasoning, &content, !sent_any) else {
            continue;
        };
        sent_any = true;
        if body_tx.send(frame).is_err() {
            // Client hung up — stop paying for tokens nobody reads.
            upstream.abort();
            aborted = true;
            break;
        }
    }

    if !aborted {
        let (reasoning, content) = splitter.finish();
        if let Some(frame) = tail_frame(&reasoning, &content, !sent_any) {
            let _ = body_tx.send(frame);
        }
        match upstream.await {
            Ok(Ok(())) => {
                let reason = finish_reason.as_deref().unwrap_or("stop");
                let _ = body_tx.send(final_frame(&content, reason));
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

// ── Legacy `/v1/completions` (deprecated OpenAI text-completion) ────────

#[derive(Debug, Deserialize)]
struct LegacyCompletionRequest {
    model: String,
    #[serde(default)]
    prompt: Option<LegacyPrompt>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    stream: Option<bool>,
}

/// The legacy `prompt` field: a single string or a list of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LegacyPrompt {
    Text(String),
    List(Vec<String>),
}

impl LegacyPrompt {
    fn into_text(self) -> String {
        match self {
            LegacyPrompt::Text(text) => text,
            LegacyPrompt::List(parts) => parts.join("\n"),
        }
    }
}

/// Legacy text completion, synthesized on top of the chat path: the prompt
/// becomes a single user message, and the answer is shaped back as a
/// `text_completion`. Works for every backend (DeepSeek included), unlike a
/// raw upstream passthrough. Any `<think>` reasoning is stripped (legacy
/// completions have no field for it).
async fn legacy_completions(
    request: Request<Incoming>,
    context: &Arc<ServerContext>,
) -> Response<ApiBody> {
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let wire: LegacyCompletionRequest = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request(format!("invalid request body: {error}")),
            );
        }
    };

    let (resolved, provider) = match resolve_backend(&wire.model, context) {
        Ok(pair) => pair,
        Err(response) => return *response,
    };

    let prompt = wire.prompt.map(LegacyPrompt::into_text).unwrap_or_default();
    let internal = CompletionRequest {
        messages: vec![ChatMessage::user(&prompt)],
        model: resolved.internal_model().to_string(),
        temperature: wire.temperature.unwrap_or(context.defaults.temperature),
        max_tokens: wire.max_tokens.unwrap_or(context.defaults.max_tokens),
        stream: wire.stream.unwrap_or(false),
    };
    let public_model = wire.model.clone();
    let meta = CompletionMeta::generate(&public_model);
    let streaming = internal.stream;

    let mut response = if streaming {
        legacy_stream(provider, resolved.is_deepseek(), internal, meta)
    } else {
        legacy_blocking(provider, resolved.is_deepseek(), &prompt, internal, meta).await
    };
    response.extensions_mut().insert(LogDetail(format!(
        "legacy model={public_model} → {}{}",
        resolved.internal_model(),
        if streaming { " (stream)" } else { "" }
    )));
    response
}

/// One `text_completion` choice object.
fn legacy_choice(text: &str, finish_reason: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "index": 0,
        "text": text,
        "finish_reason": finish_reason,
        "logprobs": serde_json::Value::Null,
    })
}

async fn legacy_blocking(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    prompt: &str,
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    let response = match run_blocking_completion(&provider, is_deepseek, request).await {
        Ok(response) => response,
        Err(error_response) => return error_response,
    };
    let (_reasoning, text) = openai_compat::split_reasoning(&response.content);
    let usage = response
        .usage
        .unwrap_or_else(|| openai_compat::estimated_usage(&[prompt], &text));
    let finish = response.finish_reason.unwrap_or_else(|| "stop".to_string());
    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "id": meta.id,
            "object": "text_completion",
            "created": meta.created,
            "model": meta.model,
            "choices": [legacy_choice(&text, Some(&finish))],
            "usage": usage,
        }),
    )
}

fn legacy_stream(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    sse_stream_response(move |body_tx| {
        bridge_legacy_stream(provider, is_deepseek, request, meta, body_tx)
    })
}

/// Legacy `text_completion` frame shaping over [`drive_sse_bridge`].
/// Reasoning is dropped (the legacy shape has no field for it), so only
/// the post-`<think>` content is streamed as `text`; the splitter's tail
/// rides in the final frame together with the finish reason.
async fn bridge_legacy_stream(
    provider: Arc<dyn LLMProvider>,
    is_deepseek: bool,
    request: CompletionRequest,
    meta: CompletionMeta,
    body_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
) {
    let text_chunk = |text: &str, finish: Option<&str>| {
        sse_data(&serde_json::json!({
            "id": meta.id,
            "object": "text_completion",
            "created": meta.created,
            "model": meta.model,
            "choices": [legacy_choice(text, finish)],
        }))
    };
    drive_sse_bridge(
        provider,
        is_deepseek,
        request,
        body_tx,
        |_reasoning, content, _is_first| {
            if content.is_empty() {
                return None;
            }
            Some(text_chunk(content, None))
        },
        |_reasoning, _content, _is_first| None,
        |content, reason| text_chunk(content, Some(reason)),
    )
    .await;
}

// ── Passthrough (`/v1/embeddings`, `/v1/rerank`) ───────────────────────

/// Endpoints pooprusteek doesn't model itself (embeddings, reranking) are
/// forwarded verbatim to the upstream `/providers` entry that serves the
/// requested model — the only backend that can actually answer them. The
/// request body's `model` is rewritten to the resolved sub-model; the
/// upstream's status + body come back untouched. The built-in DeepSeek web
/// backend has no such endpoints, so those models return a clear error.
async fn passthrough(
    request: Request<Incoming>,
    context: &Arc<ServerContext>,
    path_suffix: &str,
) -> Response<ApiBody> {
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let mut payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request(format!("invalid request body: {error}")),
            );
        }
    };
    let Some(model_id) = payload.get("model").and_then(serde_json::Value::as_str) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &ErrorResponse::invalid_request("request has no `model` field"),
        );
    };
    let model_id = model_id.to_string();

    let resolved = match catalog::resolve_model(
        &model_id,
        context.deepseek.is_some(),
        &context.entries,
        &context.models.snapshot(),
    ) {
        Ok(resolved) => resolved,
        Err(message) => return model_not_found(message),
    };
    let entry = match resolved {
        ResolvedModel::Entry { entry } => entry,
        ResolvedModel::Deepseek { .. } => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request(format!(
                    "the built-in DeepSeek backend has no {path_suffix} endpoint — pick a /providers model"
                )),
            );
        }
    };
    if entry.protocol != ProviderProtocol::Openai {
        return json_response(
            StatusCode::BAD_REQUEST,
            &ErrorResponse::invalid_request(format!(
                "{path_suffix} passthrough is only supported for OpenAI-compatible providers (got '{}' on '{}')",
                entry.protocol.label(),
                entry.name,
            )),
        );
    }

    // Forward with the sub-model the caller actually asked for.
    payload["model"] = serde_json::json!(entry.model);
    let url = format!("{}{path_suffix}", entry.base_url);
    let mut upstream = context.http_client.post(&url).json(&payload);
    if let Some(key) = &entry.api_key {
        upstream = upstream.bearer_auth(key);
    }

    match upstream.send().await {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let upstream_bytes = response.bytes().await.unwrap_or_default();
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(full_body(upstream_bytes))
                .expect("static response parts are valid")
        }
        Err(error) => json_response(
            StatusCode::BAD_GATEWAY,
            &ErrorResponse::server_error(format!(
                "upstream '{}' {path_suffix} request failed: {error}",
                entry.name
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::app::events::AppEvent;
    use crate::config::{ProviderEntry, ProviderProtocol, ServerApi};
    use crate::provider::openai_compat::RequestDefaults;
    use crate::server::{ServerSettings, spawn};
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
        let path = request.uri().path().to_string();
        let body = request
            .into_body()
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

        // Non-chat endpoints the gateway forwards verbatim.
        if path.ends_with("/embeddings") {
            let payload = serde_json::json!({
                "object": "list",
                "model": parsed["model"],
                "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
                "usage": {"prompt_tokens": 2, "total_tokens": 2},
            });
            let response = Response::builder()
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(payload.to_string())))
                .expect("static response parts are valid");
            return Ok(response);
        }

        // Chat completions — the model requested (already the sub-model the
        // gateway rewrote to) selects the canned answer: `think` returns an
        // inline `<think>` block, anything else a plain greeting.
        let model = parsed["model"].as_str().unwrap_or("?").to_string();
        let reasoning = model.contains("think");
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
            let content = if reasoning {
                "<think>inner thoughts</think>Final answer.".to_string()
            } else {
                format!("Hello from {model}")
            };
            let payload = serde_json::json!({
                "id": "up-1",
                "object": "chat.completion",
                "created": 1,
                "model": parsed["model"],
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
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
            defaults: RequestDefaults {
                temperature: 0.7,
                max_tokens: 256,
            },
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
        let first: serde_json::Value = serde_json::from_str(payloads[0]).expect("first chunk json");
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
        let last: serde_json::Value =
            serde_json::from_str(payloads[payloads.len() - 2]).expect("terminal chunk json");
        assert_eq!(last["choices"][0]["finish_reason"], "stop");

        handle.request_shutdown();
    }

    /// Spin up gateway + mock upstream and return the base URL + shutdown
    /// handle. Shared setup for the endpoint-specific tests below.
    async fn start_gateway() -> (String, crate::server::ServerHandle) {
        let upstream = spawn_mock_upstream().await;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
        let handle = spawn(gateway_settings(upstream), models, 1, event_tx);
        let addr = loop {
            match event_rx.recv().await {
                Some(AppEvent::ServerStarted { addr, .. }) => break addr,
                Some(_) => continue,
                None => panic!("server ended before ServerStarted"),
            }
        };
        (format!("http://{addr}"), handle)
    }

    /// An inline `<think>` block from the upstream is hoisted into
    /// `reasoning_content`, leaving `content` clean (the reviewer's #2).
    #[tokio::test]
    async fn gateway_hoists_think_block_into_reasoning_content() {
        let (base, handle) = start_gateway().await;
        let completion: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "mock/think",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .send()
            .await
            .expect("completion request")
            .json()
            .await
            .expect("completion json");
        let message = &completion["choices"][0]["message"];
        assert_eq!(message["content"], "Final answer.");
        assert_eq!(message["reasoning_content"], "inner thoughts");
        handle.request_shutdown();
    }

    /// Legacy `/v1/completions`: prompt → chat → `text_completion` shape.
    #[tokio::test]
    async fn gateway_legacy_completions_returns_text_completion() {
        let (base, handle) = start_gateway().await;
        let completion: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/v1/completions"))
            .json(&serde_json::json!({"model": "mock/custom", "prompt": "hello"}))
            .send()
            .await
            .expect("legacy request")
            .json()
            .await
            .expect("legacy json");
        assert_eq!(completion["object"], "text_completion");
        assert_eq!(completion["choices"][0]["text"], "Hello from custom");
        assert_eq!(completion["choices"][0]["finish_reason"], "stop");
        handle.request_shutdown();
    }

    /// `/v1/embeddings` forwards to the entry upstream with the sub-model
    /// rewritten in; the upstream's body comes back verbatim.
    #[tokio::test]
    async fn gateway_embeddings_passthrough_rewrites_model() {
        let (base, handle) = start_gateway().await;
        let embeddings: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/v1/embeddings"))
            .json(&serde_json::json!({"model": "mock/bge", "input": "x"}))
            .send()
            .await
            .expect("embeddings request")
            .json()
            .await
            .expect("embeddings json");
        assert_eq!(embeddings["object"], "list");
        // The upstream echoes the model it received — proving the sub-model
        // (`bge`), not the wire id (`mock/bge`), was forwarded.
        assert_eq!(embeddings["model"], "bge");
        assert_eq!(
            embeddings["data"][0]["embedding"].as_array().map(Vec::len),
            Some(3)
        );
        handle.request_shutdown();
    }

    /// An unknown model on a passthrough endpoint is a 404, not a 502.
    #[tokio::test]
    async fn gateway_embeddings_unknown_model_is_404() {
        let (base, handle) = start_gateway().await;
        let response = reqwest::Client::new()
            .post(format!("{base}/v1/embeddings"))
            .json(&serde_json::json!({"model": "nope", "input": "x"}))
            .send()
            .await
            .expect("embeddings request");
        assert_eq!(response.status(), 404);
        handle.request_shutdown();
    }
}
