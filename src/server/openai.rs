//! The OpenAI Chat Completions dialect of the API server:
//! `GET /v1/models` + `POST /v1/chat/completions` (the `/v1`-less spellings
//! work too). Wire types and conversions come from
//! `provider::openai_compat`; this file is routing + execution:
//! resolve the model id to a backend, run the completion on a fresh fork,
//! and (for streams) bridge `CompletionChunk`s into SSE frames ending with
//! the literal `data: [DONE]`. A request with `tools` takes the tool path
//! (`server::tools`): the reply is buffered, a call cut off by the output
//! limit is continued, and the calls go back as `tool_calls`.

use super::catalog::{self, ResolvedModel};
use super::http::{ApiBody, LogDetail, ServerContext, full_body, json_response};
use super::routing;
use super::tools::{BridgedReply, ToolBridge, flatten_history};
use crate::agent::continuation::continue_cut_off;
use crate::agent::stream::{
    AFTER_STOP_GRACE, MAX_STREAM_BYTES, STREAM_IDLE_TIMEOUT, StreamVerdict, collect_stream,
    idle_timeout_message, oversized_stream_message,
};
use crate::config::ProviderProtocol;
use crate::error::AppError;
use crate::provider::failure::FailureClass;
use crate::provider::openai_compat::{
    self, ChatCompletionRequest, ChatCompletionResponse, Choice, CompletionMeta, ErrorResponse,
    MessageContent, ReasoningStreamSplitter, WireToolCall,
};
use crate::provider::{
    ChatMessage, CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider, Usage,
};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;

/// Requests are buffered before parsing; cap them so a misbehaving client
/// can't balloon memory. 16 MiB fits any realistic chat history.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Выбранный бэкенд: провайдер, модель для журнала и признак «идёт через шлюз».
struct Chosen {
    provider: Arc<dyn LLMProvider>,
    model: String,
    routed: bool,
}

/// A resolved backend, or a ready-to-return error response (boxed — see
/// [`resolve_backend`]).
type BackendResult = Result<Chosen, Box<Response<ApiBody>>>;

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
async fn read_body(request: Request<Incoming>) -> Result<Bytes, Box<Response<ApiBody>>> {
    http_body_util::Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|_| {
            Box::new(json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorResponse::invalid_request(format!(
                    "request body unreadable or larger than {MAX_BODY_BYTES} bytes"
                )),
            ))
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
    let gateway = context.gateway.get();
    if gateway.is_enabled() {
        return resolve_routed(&gateway, model_id, context);
    }
    let resolved = catalog::resolve_model(
        model_id,
        context.deepseek.is_some(),
        &context.entries,
        &context.models.snapshot(),
    )
    .map_err(|message| Box::new(model_not_found(message)))?;

    let provider: Arc<dyn LLMProvider> = match &resolved {
        ResolvedModel::Deepseek { .. } => match &context.deepseek {
            Some(deepseek) => deepseek.fork(),
            // Ответ 503, а не паника в задаче соединения.
            None => {
                return Err(Box::new(
                    UpstreamError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "the DeepSeek backend is not available".to_string(),
                    )
                    .response(),
                ));
            }
        },
        ResolvedModel::Entry { entry } => {
            crate::provider::build_entry_provider(entry).map_err(|error| {
                Box::new(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorResponse::server_error(format!(
                        "failed to initialize provider '{}': {}",
                        entry.name,
                        routing::scrub(&error.to_string())
                    )),
                ))
            })?
        }
    };
    Ok(Chosen {
        provider,
        model: resolved.internal_model().to_string(),
        routed: false,
    })
}

/// Адрес через шлюз: каталог проверяет `account/model` и `auto/<route>`, шлюз выбирает аккаунт.
/// Адрес, базовый URL и заголовки аккаунта клиент задать не может.
fn resolve_routed(
    gateway: &crate::gateway::handle::GatewayHandle,
    model_id: &str,
    context: &ServerContext,
) -> BackendResult {
    use crate::gateway::handle::{BuildState, CallSource, ResolveError};
    match gateway.build_state() {
        BuildState::Ready => {}
        BuildState::Starting => return Err(Box::new(starting_response())),
        BuildState::Failed(reason) => return Err(Box::new(failed_response(&reason))),
    }
    let snapshot = gateway.snapshot();
    let address = routing::check_address(&snapshot, &context.models.snapshot(), model_id)
        .map_err(|message| Box::new(model_not_found(message)))?;
    match gateway.provider_for_model_id(&address, CallSource::Serve) {
        Ok(provider) => Ok(Chosen {
            provider,
            model: address,
            routed: true,
        }),
        Err(ResolveError::Starting) => Err(Box::new(starting_response())),
        Err(ResolveError::Failed(reason)) => Err(Box::new(failed_response(&reason))),
        Err(ResolveError::Unknown(_)) => Err(Box::new(model_not_found(format!(
            "model '{}' not found — see GET /v1/models for what this server exposes",
            model_id.trim()
        )))),
        Err(error @ ResolveError::Disabled) => Err(Box::new(
            UpstreamError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string()).response(),
        )),
    }
}

/// Шлюз собирается: клиент должен повторить, а не получить «модели нет».
fn starting_response() -> Response<ApiBody> {
    let mut error = UpstreamError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "the gateway is still starting, retry in a moment".to_string(),
    );
    error.code = Some("gateway_starting");
    error.retry_after = Some(std::time::Duration::from_secs(2));
    error.response()
}

/// Заголовки `x-pooprusteek-*` по тому, куда ушёл вызов. Без шлюза ничего не добавляется.
/// Ошибочный ответ их не получает: «ответил такой-то» про отказ было бы неправдой.
fn add_route_headers(response: &mut Response<ApiBody>, provider: &Arc<dyn LLMProvider>) {
    if !response.status().is_success() {
        return;
    }
    if let Some(served) = provider.served_by() {
        for (name, value) in routing::route_headers(&served) {
            response.headers_mut().insert(name, value);
        }
    }
}

fn models_response(context: &ServerContext) -> Response<ApiBody> {
    let gateway = context.gateway.get();
    if gateway.is_enabled() {
        // Пустой список во время сборки клиент запомнил бы как «моделей нет».
        match gateway.build_state() {
            crate::gateway::handle::BuildState::Ready => {}
            crate::gateway::handle::BuildState::Starting => return starting_response(),
            crate::gateway::handle::BuildState::Failed(reason) => return failed_response(&reason),
        }
    }
    let ids = if gateway.is_enabled() {
        routing::model_ids(&gateway.snapshot(), &context.models.snapshot())
    } else {
        catalog::list_model_ids(
            context.deepseek.is_some(),
            &context.entries,
            &context.models.snapshot(),
        )
    };
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
        Err(response) => return *response,
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

    let bridge = match ToolBridge::from_wire(wire.tools.as_ref(), wire.tool_choice.as_ref()) {
        Ok(bridge) => bridge,
        Err(message) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request(message),
            );
        }
    };

    let Chosen {
        provider,
        model: chosen_model,
        routed,
    } = match resolve_backend(&wire.model, context) {
        Ok(chosen) => chosen,
        Err(response) => return *response,
    };
    let include_usage = wants_usage(&body);

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
    internal.model = chosen_model.clone();
    // Родной протокол знает вызовы в истории; остальным они нужны текстом.
    let native = provider.native_tools();
    if !native {
        flatten_history(&mut internal.messages);
    }
    if let Some(bridge) = &bridge {
        if native {
            bridge.declare(&mut internal);
        } else {
            bridge.prepare(&mut internal);
        }
    }

    let meta = CompletionMeta::generate(&public_model);
    let streaming = internal.stream;
    let with_tools = bridge.is_some();
    let observer = Arc::clone(&provider);
    let mut response = match (bridge, streaming, routed) {
        (Some(bridge), true, false) => tool_stream(provider, internal, bridge, meta),
        (Some(bridge), true, true) => tool_stream_held(provider, internal, bridge, meta).await,
        (Some(bridge), false, _) => tool_blocking(provider, internal, bridge, meta).await,
        (None, true, false) => stream_completion(provider, internal, meta, include_usage),
        (None, true, true) => held_stream_completion(provider, internal, meta, include_usage).await,
        (None, false, _) => blocking_completion(provider, internal, meta).await,
    };
    if routed {
        add_route_headers(&mut response, &observer);
    }
    // Enriches the proxy access log; free otherwise.
    response.extensions_mut().insert(LogDetail(format!(
        "model={public_model} → {chosen_model}{}{}",
        if streaming { " (stream)" } else { "" },
        if with_tools { " (tools)" } else { "" }
    )));
    response
}

/// Клиент просил расход в потоке: `stream_options.include_usage`.
fn wants_usage(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Probe {
        stream_options: Option<StreamOptions>,
    }
    #[derive(Deserialize)]
    struct StreamOptions {
        include_usage: Option<bool>,
    }
    serde_json::from_slice::<Probe>(body)
        .ok()
        .and_then(|probe| probe.stream_options)
        .and_then(|options| options.include_usage)
        .unwrap_or(false)
}

/// Как часто пустой SSE-комментарий держит соединение, пока ответ с
/// инструментами копится: клиент не должен оборвать его по простою.
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(10);

/// Пустой SSE-комментарий для [`KEEPALIVE`].
const KEEPALIVE_FRAME: &[u8] = b": keep-alive\n\n";

/// Последний кадр удачного стрима.
const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

/// Ошибка апстрима и её HTTP-код: клиент должен отличать «подожди» (429),
/// «не дождались» (504) и «апстрим сломан» (502) — всё было 500.
const UPSTREAM_AUTH_MESSAGE: &str =
    "the upstream provider rejected this server's credentials; see the server log";

const UPSTREAM_UNREACHABLE_MESSAGE: &str = "the upstream provider could not be reached";

struct UpstreamError {
    status: StatusCode,
    message: String,
    /// Сколько апстрим просил подождать: уходит клиенту заголовком `Retry-After`.
    retry_after: Option<std::time::Duration>,
    /// Тег ошибки в теле: по нему клиенты OpenAI решают, повторять ли.
    kind: &'static str,
    code: Option<&'static str>,
}

impl UpstreamError {
    fn new(status: StatusCode, message: String) -> Self {
        let (kind, code) = if status == StatusCode::TOO_MANY_REQUESTS {
            ("rate_limit_error", Some("rate_limit_exceeded"))
        } else {
            ("server_error", None)
        };
        Self {
            status,
            message,
            retry_after: None,
            kind,
            code,
        }
    }

    fn from_app(error: &AppError) -> Self {
        // URL апстрима бывает с ключом в адресе — клиенту `/serve` он не нужен.
        let mut message = error.to_string();
        if let AppError::Http(http) = error
            && let Some(url) = http.url()
        {
            message = message.replace(url.as_str(), "<upstream>");
        }
        // Остальное (шлюз прячет ту же ошибку в `AppError::Upstream`) скрабит `body()`.
        Self::from_class(message, error.failure_class(), error.retry_after())
    }

    /// Код и тег по классу отказа; без класса — 502.
    fn from_class(
        message: String,
        class: Option<FailureClass>,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        let mut mapped = Self::new(StatusCode::BAD_GATEWAY, message);
        mapped.retry_after = retry_after;
        let Some(class) = class else {
            return mapped;
        };
        let (status, kind, code) = match class {
            FailureClass::RateLimit { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                Some("rate_limit_exceeded"),
            ),
            // Квота кончилась: ждать секунды бесполезно, `Retry-After` не шлём.
            FailureClass::QuotaExhausted { .. } | FailureClass::SpendCap => {
                mapped.retry_after = None;
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "insufficient_quota",
                    Some("insufficient_quota"),
                )
            }
            FailureClass::Overloaded => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                Some("overloaded"),
            ),
            FailureClass::Timeout => (StatusCode::GATEWAY_TIMEOUT, "server_error", None),
            // Ключ апстрима — забота сервера. Текст отказа может раскрыть ключ или аккаунт.
            FailureClass::Auth | FailureClass::AccountBlocked => {
                mapped.message = UPSTREAM_AUTH_MESSAGE.to_string();
                (
                    StatusCode::BAD_GATEWAY,
                    "server_error",
                    Some("upstream_auth"),
                )
            }
            FailureClass::ContextTooLong => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                Some("context_length_exceeded"),
            ),
            FailureClass::ModelNotFound => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                Some("model_not_found"),
            ),
            FailureClass::ContentFilter | FailureClass::BadRequest => {
                (StatusCode::BAD_REQUEST, "invalid_request_error", None)
            }
            // Сетевая ошибка называет адрес аккаунта: клиенту хватает класса.
            FailureClass::Network => {
                mapped.message = UPSTREAM_UNREACHABLE_MESSAGE.to_string();
                return mapped;
            }
            FailureClass::Server | FailureClass::Protocol | FailureClass::Cancelled => {
                return mapped;
            }
        };
        mapped.status = status;
        mapped.kind = kind;
        mapped.code = code;
        mapped
    }

    fn idle() -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, idle_timeout_message())
    }

    /// Поток закрылся без конца ответа — это обрыв, а не «stop».
    fn closed_early() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "upstream closed the stream before finishing the reply".to_string(),
        )
    }

    fn task_failed(what: &str, join_error: &tokio::task::JoinError) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::util::join_error_text(what, join_error),
        )
    }

    fn from_verdict(
        verdict: StreamVerdict,
        failure: Option<&crate::agent::stream::StreamFailure>,
    ) -> Option<Self> {
        match verdict {
            StreamVerdict::IdleTimeout => Some(Self::idle()),
            StreamVerdict::Failed(error) => Some(Self::from_class(
                error,
                failure.map(|f| f.class.clone()),
                failure.and_then(|f| f.retry_after),
            )),
            StreamVerdict::ClosedWithoutStop => Some(Self::closed_early()),
            StreamVerdict::Ok | StreamVerdict::Finished(_) => None,
        }
    }

    /// Тело в духе OpenAI: по `rate_limit_error` клиенты решают, повторять ли.
    fn body(&self) -> ErrorResponse {
        ErrorResponse {
            error: openai_compat::ErrorBody {
                message: routing::scrub(&self.message),
                kind: self.kind.to_string(),
                code: self.code.map(str::to_string),
            },
        }
    }

    fn response(&self) -> Response<ApiBody> {
        let mut response = json_response(self.status, &self.body());
        if let Some(wait) = self.retry_after
            && let Ok(value) = header::HeaderValue::from_str(&wait.as_secs().max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }

    /// Та же ошибка кадром SSE: статус 200 уже ушёл клиенту.
    fn frame(&self) -> Bytes {
        sse_data(&self.body())
    }
}

/// Ответ с инструментами целиком: вызов виден только в готовом ответе.
/// Оборванный лимитом вызов дописывается, неразобранный — переспрашивается раз.
async fn run_tool_completion(
    provider: &Arc<dyn LLMProvider>,
    request: &CompletionRequest,
    bridge: &ToolBridge,
) -> Result<(BridgedReply, String), UpstreamError> {
    // Гард: клиент ушёл — задачу гасят, а серверная сессия всё равно удаляется.
    let discard = crate::provider::SessionDiscard::new(Arc::clone(provider));
    let mut result = tool_attempt(provider, request, bridge).await;
    if let Ok((reply, _, raw)) = &result
        && reply.calls.is_empty()
        && !reply.errors.is_empty()
    {
        let mut retry = request.clone();
        retry.messages.push(ChatMessage::assistant(raw));
        retry.messages.push(ChatMessage::user(
            &crate::agent::runner::malformed_tool_feedback(&reply.errors),
        ));
        // Переспрос сорвался — первый ответ всё же лучше, чем ничего.
        match tool_attempt(provider, &retry, bridge).await {
            Ok(second) => result = Ok(second),
            Err(error) => tracing::debug!("server: tool retry failed: {}", error.message),
        }
    }
    discard.now().await;
    result.map(|(reply, finish, _)| (reply, finish))
}

/// Один запрос ответа с инструментами: разобранный ответ, причина конца и
/// сырой текст (для переспроса).
async fn tool_attempt(
    provider: &Arc<dyn LLMProvider>,
    request: &CompletionRequest,
    bridge: &ToolBridge,
) -> Result<(BridgedReply, String, String), UpstreamError> {
    let outcome = collect_stream(provider, request.clone(), |_| {}).await;
    if let Some(error) = UpstreamError::from_verdict(outcome.verdict(), outcome.failure.as_ref()) {
        return Err(error);
    }
    if !request.tools.is_empty() {
        let reply = ToolBridge::native_reply(&outcome.text, &outcome.tool_calls);
        let finish = if reply.calls.is_empty() {
            outcome.stop_reason.unwrap_or_else(|| "stop".to_string())
        } else {
            "tool_calls".to_string()
        };
        return Ok((reply, finish, outcome.text));
    }
    let stop = outcome.stop_reason.clone();
    let continued = continue_cut_off(
        provider,
        request,
        outcome.text,
        |text| bridge.cut_at(text, request),
        |_| {},
    )
    .await;
    let reply = bridge.parse(&continued.text, request);
    let finish = match (&continued.failure, reply.calls.is_empty()) {
        (_, false) => "tool_calls".to_string(),
        // Вызов так и остался оборванным — клиент должен знать, что ответ неполон.
        (Some(failure), true) => {
            tracing::debug!("server: tool reply left cut off: {failure}");
            "length".to_string()
        }
        (None, true) => stop.unwrap_or_else(|| "stop".to_string()),
    };
    Ok((reply, finish, continued.text))
}

async fn tool_blocking(
    provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    bridge: ToolBridge,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    match run_tool_completion(&provider, &request, &bridge).await {
        Ok((reply, finish)) => json_response(
            StatusCode::OK,
            &tool_response(&request, reply, finish, &meta),
        ),
        Err(error) => error.response(),
    }
}

fn tool_response(
    request: &CompletionRequest,
    reply: BridgedReply,
    finish: String,
    meta: &CompletionMeta,
) -> ChatCompletionResponse {
    let prompt_texts: Vec<&str> = request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    let usage = openai_compat::estimated_usage(&prompt_texts, &reply.content);
    let has_calls = !reply.calls.is_empty();
    ChatCompletionResponse {
        id: meta.id.clone(),
        object: "chat.completion".to_string(),
        created: meta.created,
        model: meta.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: openai_compat::ChatCompletionMessage {
                role: "assistant".to_string(),
                // У ответа из одних вызовов текста нет — как у OpenAI.
                content: if has_calls && reply.content.is_empty() {
                    MessageContent::Null
                } else {
                    MessageContent::Text(reply.content)
                },
                name: None,
                tool_call_id: None,
                reasoning_content: reply.reasoning,
                tool_calls: has_calls.then_some(reply.calls),
            },
            finish_reason: Some(finish),
        }],
        usage: Some(usage.into()),
    }
}

/// Стрим с инструментами: пока ответ копится — keep-alive, затем готовые кадры.
fn tool_stream(
    provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    bridge: ToolBridge,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    sse_stream_response(move |body_tx| async move {
        // Гард: ушедший клиент гасит и задачу ответа, а не оставляет её сиротой.
        let mut work = crate::util::AbortOnDrop::spawn(async move {
            run_tool_completion(&provider, &request, &bridge).await
        });
        let joined = loop {
            match tokio::time::timeout(KEEPALIVE, &mut work).await {
                Ok(joined) => break joined,
                Err(_elapsed) => {
                    if body_tx.send(Bytes::from_static(KEEPALIVE_FRAME)).is_err() {
                        return;
                    }
                }
            }
        };
        let frames = match joined {
            Ok(Ok((reply, finish))) => tool_frames(reply, &finish, &meta),
            Ok(Err(error)) => vec![error.frame()],
            Err(join_error) => {
                vec![UpstreamError::task_failed("the completion task", &join_error).frame()]
            }
        };
        for frame in frames {
            let _ = body_tx.send(frame);
        }
    })
}

/// Готовый ответ кадрами SSE: текст, затем все вызовы, затем конец.
fn tool_frames(reply: BridgedReply, finish: &str, meta: &CompletionMeta) -> Vec<Bytes> {
    let reasoning = reply.reasoning.unwrap_or_default();
    let mut frames = vec![sse_data(&openai_compat::split_delta_chunk(
        &reasoning,
        &reply.content,
        true,
        meta,
    ))];
    if !reply.calls.is_empty() {
        let calls: Vec<WireToolCall> = reply
            .calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| WireToolCall {
                index: Some(index as u32),
                ..call
            })
            .collect();
        let mut chunk = openai_compat::split_delta_chunk("", "", false, meta);
        chunk.choices[0].delta.tool_calls = Some(calls);
        frames.push(sse_data(&chunk));
    }
    frames.push(sse_data(&openai_compat::final_chunk(finish, meta)));
    frames.push(Bytes::from_static(DONE_FRAME));
    frames
}

async fn blocking_completion(
    provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    let prompt_texts: Vec<String> = request
        .messages
        .iter()
        .map(|message| message.content.clone())
        .collect();
    let response = match run_blocking_completion(&provider, request).await {
        Ok(response) => response,
        Err(error_response) => return *error_response,
    };
    let prompt_refs: Vec<&str> = prompt_texts.iter().map(String::as_str).collect();
    let fallback = openai_compat::estimated_usage(&prompt_refs, &response.content);
    json_response(
        StatusCode::OK,
        &openai_compat::response_to_openai(&response, fallback, &meta),
    )
}

/// Обычный ответ и удаление одноразовой серверной сессии; ошибка — готовый
/// ответ с кодом из [`UpstreamError`]. Общая половина чата и legacy.
async fn run_blocking_completion(
    provider: &Arc<dyn LLMProvider>,
    request: CompletionRequest,
) -> Result<CompletionResponse, Box<Response<ApiBody>>> {
    // Гард: hyper роняет future, если клиент ушёл, — сессия всё равно удаляется.
    let discard = crate::provider::SessionDiscard::new(Arc::clone(provider));
    let result = provider.complete(request).await;
    discard.now().await;
    result.map_err(|error| Box::new(UpstreamError::from_app(&error).response()))
}

/// Streaming path: respond immediately with an SSE body fed by a bridge
/// task. Errors after the 200 has been committed travel in-stream as an
/// `{"error": …}` data line — the same convention OpenAI-compatible
/// proxies use.
fn stream_completion(
    provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    meta: CompletionMeta,
    include_usage: bool,
) -> Response<ApiBody> {
    sse_stream_response(move |body_tx| async move {
        let pump = Pump::new(VecDeque::new(), include_usage, &meta);
        chat_bridge(Upstream::start(provider, request), pump, meta, body_tx).await;
    })
}

/// Поток через шлюз: ответа нет, пока апстрим не дал первый чанк; отказ до него — честный HTTP-код.
/// Заголовки `x-pooprusteek-*` называют аккаунт, который действительно отвечает.
async fn held_stream_completion(
    provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    meta: CompletionMeta,
    include_usage: bool,
) -> Response<ApiBody> {
    let mut upstream = Upstream::start(provider, request);
    let backlog = match await_first(&mut upstream).await {
        Ok(backlog) => backlog,
        Err(error) => return error.response(),
    };
    let pump = Pump::new(backlog, include_usage, &meta);
    sse_stream_response(move |body_tx| chat_bridge(upstream, pump, meta, body_tx))
}

/// Ответ с инструментами через шлюз: копится целиком и уходит одним ответом, поэтому отказ
/// остаётся честным кодом, а переход на запасной аккаунт виден в заголовках.
async fn tool_stream_held(
    provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    bridge: ToolBridge,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    match run_tool_completion(&provider, &request, &bridge).await {
        Ok((reply, finish)) => {
            let mut body = Vec::new();
            for frame in tool_frames(reply, &finish, &meta) {
                body.extend_from_slice(&frame);
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(full_body(body))
                .expect("static response parts are valid")
        }
        Err(error) => error.response(),
    }
}

/// Задача апстрима, её канал чанков и уборка за ней.
struct Upstream {
    task: crate::util::AbortOnDrop<crate::error::AppResult<()>>,
    chunks: tokio::sync::mpsc::UnboundedReceiver<CompletionChunk>,
    discard: crate::provider::SessionDiscard,
}

impl Upstream {
    fn start(provider: Arc<dyn LLMProvider>, request: CompletionRequest) -> Self {
        let discard = crate::provider::SessionDiscard::new(Arc::clone(&provider));
        let (chunk_tx, chunks) = tokio::sync::mpsc::unbounded_channel();
        // Гард: ушедший клиент гасит и задачу апстрима, а не оставляет её сиротой.
        let task = crate::util::AbortOnDrop::spawn(async move {
            provider.complete_stream(request, chunk_tx).await
        });
        Self {
            task,
            chunks,
            discard,
        }
    }
}

/// Первый чанк, по которому можно отвечать: текст, вызов или конец. Чанки только с расходом
/// ждут вместе с ним. Ошибка до него — ответ-ошибка, а не 200 с обрывом.
async fn await_first(upstream: &mut Upstream) -> Result<VecDeque<CompletionChunk>, UpstreamError> {
    let mut seen = VecDeque::new();
    loop {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, upstream.chunks.recv()).await {
            Err(_elapsed) => return Err(UpstreamError::idle()),
            Ok(Some(chunk)) => {
                let decisive = !chunk.content.is_empty()
                    || !chunk.tool_calls.is_empty()
                    || chunk.finish_reason.is_some();
                seen.push_back(chunk);
                if decisive {
                    return Ok(seen);
                }
            }
            Ok(None) => {
                return match (&mut upstream.task).await {
                    Ok(Ok(())) if !seen.is_empty() => Ok(seen),
                    Ok(Ok(())) => Err(UpstreamError::closed_early()),
                    Ok(Err(error)) => Err(UpstreamError::from_app(&error)),
                    Err(join_error) => Err(UpstreamError::task_failed(
                        "the upstream stream",
                        &join_error,
                    )),
                };
            }
        }
    }
}

/// Кадр расхода для клиента, который его просил.
type UsageFrame = Box<dyn FnOnce(&Usage) -> Bytes + Send>;

/// Что насосу известно сверх канала: снятые при удержании чанки и кадр расхода.
struct Pump {
    backlog: VecDeque<CompletionChunk>,
    usage_frame: Option<UsageFrame>,
}

impl Pump {
    fn new(backlog: VecDeque<CompletionChunk>, include_usage: bool, meta: &CompletionMeta) -> Self {
        let meta = meta.clone();
        Self {
            backlog,
            usage_frame: include_usage.then(|| {
                Box::new(move |usage: &Usage| {
                    // Как у OpenAI: последний чанк с пустым `choices` и полем `usage`.
                    let mut chunk = openai_compat::final_chunk("stop", &meta);
                    chunk.choices.clear();
                    chunk.usage = Some(usage.clone().into());
                    sse_data(&chunk)
                }) as UsageFrame
            }),
        }
    }

    fn none() -> Self {
        Self {
            backlog: VecDeque::new(),
            usage_frame: None,
        }
    }
}

/// Сколько после `finish_reason` ждать чанк расхода, если клиент его просил.
const USAGE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

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
async fn chat_bridge(
    upstream: Upstream,
    pump: Pump,
    meta: CompletionMeta,
    body_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
) {
    drive_sse_bridge(
        upstream,
        pump,
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

/// Общий насос обоих SSE-мостов; диалекты дают только кадры: дельту (`None` —
/// пока нечего слать), удержанный хвост и финальный кадр. Клиент ушёл — апстрим гасится сразу.
async fn drive_sse_bridge(
    upstream: Upstream,
    mut pump: Pump,
    body_tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    mut delta_frame: impl FnMut(&str, &str, bool) -> Option<Bytes> + Send,
    tail_frame: impl FnOnce(&str, &str, bool) -> Option<Bytes> + Send,
    final_frame: impl FnOnce(&str, &str) -> Bytes + Send,
) {
    let Upstream {
        mut task,
        mut chunks,
        discard,
    } = upstream;

    let mut splitter = ReasoningStreamSplitter::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut finish_seen: Option<tokio::time::Instant> = None;
    let mut sent_any = false;
    let mut aborted = false;
    let mut received = 0usize;
    let mut last_sent = tokio::time::Instant::now();
    let wants_usage = pump.usage_frame.is_some();
    loop {
        let chunk = match pump.backlog.pop_front() {
            Some(chunk) => chunk,
            None => {
                let wait = match finish_seen {
                    Some(seen) => USAGE_WAIT.saturating_sub(seen.elapsed()),
                    None => STREAM_IDLE_TIMEOUT,
                };
                tokio::select! {
                    biased;
                    // Клиент закрыл соединение: дальше платить за токены некому.
                    () = body_tx.closed() => {
                        aborted = true;
                        break;
                    }
                    next = tokio::time::timeout(wait, chunks.recv()) => match next {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break, // апстрим закрыл канал — разбор ниже
                        Err(_elapsed) if finish_seen.is_some() => break,
                        Err(_elapsed) => {
                            let _ = body_tx.send(UpstreamError::idle().frame());
                            aborted = true;
                            break;
                        }
                    },
                }
            }
        };
        if chunk.usage.is_some() {
            // Последнее слово сервера: расход приходит один раз и не суммируется.
            usage = chunk.usage.clone();
        }
        if chunk.finish_reason.is_some() {
            finish_reason = chunk.finish_reason.clone();
        }
        received += chunk.content.len();
        if received > MAX_STREAM_BYTES {
            let error = UpstreamError::new(StatusCode::BAD_GATEWAY, oversized_stream_message());
            let _ = body_tx.send(error.frame());
            aborted = true;
            break;
        }
        // Пустой кусок или удержанный сплиттером хвост — кадра пока нет.
        let frame = if chunk.content.is_empty() {
            None
        } else {
            let (reasoning, content) = splitter.push(&chunk.content);
            delta_frame(&reasoning, &content, !sent_any)
        };
        let sent = match frame {
            Some(frame) => {
                sent_any = true;
                Some(body_tx.send(frame))
            }
            // Апстрим думает: комментарий держит соединение, иначе клиент
            // рвал его по своему сроку простоя, не увидев ни байта.
            None if last_sent.elapsed() >= KEEPALIVE => {
                Some(body_tx.send(Bytes::from_static(KEEPALIVE_FRAME)))
            }
            None => None,
        };
        match sent {
            // Клиент ушёл — не платим за токены, которые никто не прочтёт.
            Some(Err(_)) => {
                aborted = true;
                break;
            }
            Some(Ok(())) => last_sent = tokio::time::Instant::now(),
            None => {}
        }
        if finish_reason.is_some() {
            // Расход OpenAI идёт отдельным чанком после конца: ждём его, только если просили.
            if !wants_usage || usage.is_some() {
                break;
            }
            finish_seen.get_or_insert_with(tokio::time::Instant::now);
        }
    }

    if !aborted {
        let (reasoning, content) = splitter.finish();
        if let Some(frame) = tail_frame(&reasoning, &content, !sent_any) {
            let _ = body_tx.send(frame);
        }
        // После финиша ответ уже весь: шлюз, держащий соединение, не держит нас.
        let joined = match &finish_reason {
            Some(_) => tokio::time::timeout(AFTER_STOP_GRACE, &mut task)
                .await
                .unwrap_or(Ok(Ok(()))),
            None => (&mut task).await,
        };
        let failure = upstream_failure(joined);
        match finish_reason.as_deref() {
            // Причина конца пришла — ответ целый, поздняя ошибка его не портит.
            Some(reason) => {
                if let Some(failure) = failure {
                    tracing::debug!(
                        "server: upstream failed after finishing: {}",
                        failure.message
                    );
                }
                let _ = body_tx.send(final_frame(&content, reason));
                if let (Some(make), Some(usage)) = (pump.usage_frame.take(), usage.as_ref()) {
                    let _ = body_tx.send(make(usage));
                }
                let _ = body_tx.send(Bytes::from_static(DONE_FRAME));
            }
            None => {
                let failure = failure.unwrap_or_else(UpstreamError::closed_early);
                let _ = body_tx.send(failure.frame());
            }
        }
    }

    // Сначала гасим апстрим: иначе он жжёт токены, пока сессию удаляют.
    drop(task);
    discard.now().await;
}

/// Чем кончилась задача апстрима: `None` — штатно.
fn upstream_failure(
    joined: Result<crate::error::AppResult<()>, tokio::task::JoinError>,
) -> Option<UpstreamError> {
    match joined {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(UpstreamError::from_app(&error)),
        Err(join_error) => Some(UpstreamError::task_failed(
            "the upstream stream",
            &join_error,
        )),
    }
}

/// One SSE `data:` frame.
fn sse_data(payload: &impl Serialize) -> Bytes {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {json}\n\n"))
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
        Err(response) => return *response,
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

    let Chosen {
        provider,
        model: chosen_model,
        routed,
    } = match resolve_backend(&wire.model, context) {
        Ok(chosen) => chosen,
        Err(response) => return *response,
    };

    let prompt = wire.prompt.map(LegacyPrompt::into_text).unwrap_or_default();
    let internal = CompletionRequest {
        messages: vec![ChatMessage::user(&prompt)],
        tools: Vec::new(),
        model: chosen_model.clone(),
        temperature: wire.temperature.unwrap_or(context.defaults.temperature),
        max_tokens: wire.max_tokens.unwrap_or(context.defaults.max_tokens),
        stream: wire.stream.unwrap_or(false),
    };
    let public_model = wire.model.clone();
    let meta = CompletionMeta::generate(&public_model);
    let streaming = internal.stream;

    let observer = Arc::clone(&provider);
    let mut response = if streaming {
        legacy_stream(provider, internal, meta)
    } else {
        legacy_blocking(provider, &prompt, internal, meta).await
    };
    if routed {
        add_route_headers(&mut response, &observer);
    }
    response.extensions_mut().insert(LogDetail(format!(
        "legacy model={public_model} → {chosen_model}{}",
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
    prompt: &str,
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    let response = match run_blocking_completion(&provider, request).await {
        Ok(response) => response,
        Err(error_response) => return *error_response,
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
    request: CompletionRequest,
    meta: CompletionMeta,
) -> Response<ApiBody> {
    sse_stream_response(move |body_tx| bridge_legacy_stream(provider, request, meta, body_tx))
}

/// Legacy `text_completion` frame shaping over [`drive_sse_bridge`].
/// Reasoning is dropped (the legacy shape has no field for it), so only
/// the post-`<think>` content is streamed as `text`; the splitter's tail
/// rides in the final frame together with the finish reason.
async fn bridge_legacy_stream(
    provider: Arc<dyn LLMProvider>,
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
        Upstream::start(provider, request),
        Pump::none(),
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
        Err(response) => return *response,
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

    let gateway = context.gateway.get();
    if gateway.is_enabled() {
        return routed_passthrough(&gateway, context, payload, &model_id, path_suffix).await;
    }

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

    // Код и текст без URL — как у чата; имя записи подсказывает, кто сломан.
    let failed = |error: reqwest::Error| {
        let mut failure = UpstreamError::from_app(&AppError::Http(error));
        failure.message = format!(
            "upstream '{}' {path_suffix} request failed: {}",
            entry.name, failure.message
        );
        failure.response()
    };
    match upstream.send().await {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            // Оборванное тело — ошибка, а не 200 с пустотой.
            match response.bytes().await {
                Ok(upstream_bytes) => Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(full_body(upstream_bytes))
                    .expect("static response parts are valid"),
                Err(error) => failed(error),
            }
        }
        Err(error) => failed(error),
    }
}

/// `/embeddings` и `/rerank` за шлюзом: прямо на аккаунт по `<account>/<model>`, с его адресом и
/// ключом. Выбор аккаунта по маршруту здесь не имеет смысла, а подписки CLI не раздаются.
async fn routed_passthrough(
    gateway: &crate::gateway::handle::GatewayHandle,
    context: &ServerContext,
    mut payload: serde_json::Value,
    model_id: &str,
    path_suffix: &str,
) -> Response<ApiBody> {
    use crate::gateway::handle::BuildState;
    match gateway.build_state() {
        BuildState::Ready => {}
        BuildState::Starting => return starting_response(),
        BuildState::Failed(reason) => return failed_response(&reason),
    }
    if model_id.trim().starts_with("auto/") {
        return json_response(
            StatusCode::BAD_REQUEST,
            &ErrorResponse::invalid_request(format!(
                "{path_suffix} needs an `<account>/<model>` address: `auto/<route>` picks an account per request and is not supported here"
            )),
        );
    }
    let snapshot = gateway.snapshot();
    let address = match routing::check_address(&snapshot, &context.models.snapshot(), model_id) {
        Ok(address) => address,
        Err(message) => return model_not_found(message),
    };
    let Some((account, model)) = address.split_once('/') else {
        return model_not_found(format!("model '{model_id}' not found"));
    };
    let backend = crate::gateway::types::AccountId::parse(account)
        .ok()
        .and_then(|id| gateway.backend_for(&id));
    let Some(backend) = backend else {
        return model_not_found(format!("model '{model_id}' not found"));
    };
    payload["model"] = serde_json::json!(model);
    match backend.proxy_json(path_suffix, payload).await {
        Ok(reply) => Response::builder()
            .status(StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY))
            .header(header::CONTENT_TYPE, "application/json")
            .body(full_body(reply.body))
            .expect("static response parts are valid"),
        Err(error) => {
            let mut failure = UpstreamError::from_class(error.message, Some(error.class), None);
            // Отказ класса «неверный запрос» объясняем как есть, остальное сводится к классу.
            if failure.status == StatusCode::BAD_REQUEST {
                failure.kind = "invalid_request_error";
            }
            failure.response()
        }
    }
}

/// Сборка шлюза упала: повтор не поможет, пока не исправят конфиг.
fn failed_response(reason: &str) -> Response<ApiBody> {
    let mut error = UpstreamError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("the gateway failed to start: {reason}"),
    );
    error.code = Some("gateway_failed");
    error.response()
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
        // `tool…` модели отвечают вызовом: `toolcut` — оборванным, который
        // дописывается на просьбу продолжить.
        let asked_to_continue = parsed["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|last| last["content"].as_str())
            .is_some_and(|text| text.contains("cut off by the output limit"));
        let tool_reply = match model.as_str() {
            "tool" => Some(
                "Writing.\n<tool_use><name>write</name><arguments>{\"path\": \"a\"}</arguments></tool_use>",
            ),
            "toolcut" if asked_to_continue => Some("a\"}</arguments></tool_use>"),
            "toolcut" => Some("<tool_use><name>write</name><arguments>{\"path\": \""),
            _ => None,
        };
        let response = if let Some(reply) = tool_reply {
            let chunk = serde_json::json!({
                "id": "up-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                "choices": [{"index": 0, "delta": {"content": reply}, "finish_reason": "stop"}],
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from(format!(
                    "data: {chunk}\n\ndata: [DONE]\n\n"
                ))))
        } else if streaming {
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
                tools: Default::default(),
            }],
            request_log: false,
            allowed_hosts: Vec::new(),
            gateway: Default::default(),
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

    fn tool_request(model: &str, stream: bool) -> serde_json::Value {
        serde_json::json!({
            "model": model,
            "stream": stream,
            "messages": [{"role": "user", "content": "create a"}],
            "tools": [{"type": "function", "function": {
                "name": "write",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
            }}],
        })
    }

    /// Запрос с `tools`: вызов модели возвращается клиенту как `tool_calls`.
    #[tokio::test]
    async fn gateway_returns_tool_calls() {
        let (base, handle) = start_gateway().await;
        let completion: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&tool_request("mock/tool", false))
            .send()
            .await
            .expect("completion request")
            .json()
            .await
            .expect("completion json");
        let choice = &completion["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["content"], "Writing.");
        let call = &choice["message"]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "write");
        assert_eq!(call["function"]["arguments"], r#"{"path":"a"}"#);
        handle.request_shutdown();
    }

    #[tokio::test]
    async fn gateway_streams_tool_calls_as_deltas() {
        let (base, handle) = start_gateway().await;
        let body = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&tool_request("mock/tool", true))
            .send()
            .await
            .expect("stream request")
            .text()
            .await
            .expect("stream body");
        let chunks: Vec<serde_json::Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|payload| *payload != "[DONE]")
            .map(|payload| serde_json::from_str(payload).expect("chunk json"))
            .collect();
        let call = chunks
            .iter()
            .find_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"].get(0))
            .expect("a tool_calls delta");
        assert_eq!(call["index"], 0);
        assert_eq!(call["function"]["name"], "write");
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
        assert!(body.ends_with("data: [DONE]\n\n"));
        handle.request_shutdown();
    }

    /// Вызов, оборванный лимитом, дописывается вторым запросом к модели.
    #[tokio::test]
    async fn gateway_continues_a_cut_off_tool_call() {
        let (base, handle) = start_gateway().await;
        let completion: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&tool_request("mock/toolcut", false))
            .send()
            .await
            .expect("completion request")
            .json()
            .await
            .expect("completion json");
        let call = &completion["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["arguments"], r#"{"path":"a"}"#);
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

    use super::UpstreamError;
    use crate::error::{AppError, AppResult};
    use crate::provider::{
        ChatMessage, CompletionChunk, CompletionRequest, CompletionResponse, LLMProvider,
    };
    use hyper::{StatusCode, header};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_rate_limit_is_429_with_a_rate_limit_error_body() {
        let error = UpstreamError::from_app(&AppError::RateLimited("slow down".to_string()));
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        let body = error.body();
        assert_eq!(body.error.kind, "rate_limit_error");
        assert_eq!(body.error.code.as_deref(), Some("rate_limit_exceeded"));
    }

    #[test]
    fn any_other_upstream_failure_is_502_server_error() {
        let error = UpstreamError::from_app(&AppError::Provider("boom".to_string()));
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.body().error.kind, "server_error");
    }

    fn typed(class: crate::provider::failure::FailureClass) -> AppError {
        AppError::Upstream(Box::new(crate::provider::failure::ProviderFailure {
            class,
            status: Some(StatusCode::TOO_MANY_REQUESTS),
            message: "upstream said no".to_string(),
            limits: None,
        }))
    }

    /// 429 апстрима с `Retry-After` — 429 клиенту с тем же заголовком, а не 502.
    #[test]
    fn an_upstream_429_stays_429_and_passes_retry_after_on() {
        use crate::provider::failure::FailureClass;
        let wait = Duration::from_secs(42);
        let error = UpstreamError::from_app(&typed(FailureClass::RateLimit {
            retry_after: Some(wait),
        }));
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.body().error.kind, "rate_limit_error");
        let response = error.response();
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "42");
    }

    #[test]
    fn an_upstream_429_without_a_delay_has_no_retry_after_header() {
        use crate::provider::failure::FailureClass;
        let error = UpstreamError::from_app(&typed(FailureClass::RateLimit { retry_after: None }));
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            error
                .response()
                .headers()
                .get(header::RETRY_AFTER)
                .is_none()
        );
    }

    /// Квота кончилась: 429, но без `Retry-After` — ждать секунды бесполезно.
    #[test]
    fn an_exhausted_quota_is_429_insufficient_quota_without_a_delay() {
        use crate::provider::failure::FailureClass;
        let error = UpstreamError::from_app(&typed(FailureClass::SpendCap));
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            error.body().error.code.as_deref(),
            Some("insufficient_quota")
        );
        assert!(
            error
                .response()
                .headers()
                .get(header::RETRY_AFTER)
                .is_none()
        );
    }

    /// Чужой ключ клиенту не виден: отказ авторизации апстрима — 502, не 401.
    #[test]
    fn upstream_auth_and_context_failures_map_to_honest_statuses() {
        use crate::provider::failure::FailureClass;
        let auth = UpstreamError::from_app(&typed(FailureClass::Auth));
        assert_eq!(auth.status, StatusCode::BAD_GATEWAY);
        assert_eq!(auth.body().error.code.as_deref(), Some("upstream_auth"));
        assert_eq!(auth.body().error.message, super::UPSTREAM_AUTH_MESSAGE);
        let blocked = UpstreamError::from_app(&typed(FailureClass::AccountBlocked));
        assert_eq!(blocked.body().error.message, super::UPSTREAM_AUTH_MESSAGE);
        let context = UpstreamError::from_app(&typed(FailureClass::ContextTooLong));
        assert_eq!(context.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            context.body().error.code.as_deref(),
            Some("context_length_exceeded")
        );
        let overloaded = UpstreamError::from_app(&typed(FailureClass::Overloaded));
        assert_eq!(overloaded.status, StatusCode::SERVICE_UNAVAILABLE);
        let server = UpstreamError::from_app(&typed(FailureClass::Server));
        assert_eq!(server.status, StatusCode::BAD_GATEWAY);
    }

    /// Путь с инструментами идёт через `collect_stream`: тип отказа не должен
    /// теряться по дороге (раньше он превращался в строку и в 502).
    #[tokio::test]
    async fn a_typed_failure_survives_the_tool_path() {
        use crate::provider::failure::FailureClass;
        let provider: Arc<dyn LLMProvider> = Arc::new(Scripted {
            chunks: Vec::new(),
            end: ScriptEnd::Fail(|| {
                AppError::Upstream(Box::new(crate::provider::failure::ProviderFailure {
                    class: FailureClass::RateLimit {
                        retry_after: Some(Duration::from_secs(9)),
                    },
                    status: Some(StatusCode::TOO_MANY_REQUESTS),
                    message: "slow".to_string(),
                    limits: None,
                }))
            }),
        });
        let request = CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 8,
            stream: true,
        };
        let outcome = crate::agent::stream::collect_stream(&provider, request, |_| {}).await;
        let error = UpstreamError::from_verdict(outcome.verdict(), outcome.failure.as_ref())
            .expect("a failed stream is an error");
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.retry_after, Some(Duration::from_secs(9)));
    }

    /// Таймаут — 504, а URL апстрима (с ключом в запросе) клиенту не уходит.
    #[tokio::test]
    async fn an_upstream_timeout_is_504_without_the_url() {
        // Принимает соединение и молчит: клиент упрётся в свой таймаут.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent upstream");
        let addr = listener.local_addr().expect("silent upstream addr");
        let error = reqwest::Client::new()
            .get(format!("http://{addr}/v1/models?key=secret"))
            .timeout(Duration::from_millis(50))
            .send()
            .await
            .expect_err("a silent upstream must time out");
        let mapped = UpstreamError::from_app(&AppError::Http(error));
        assert_eq!(mapped.status, StatusCode::GATEWAY_TIMEOUT);
        assert!(!mapped.message.contains("secret"), "{}", mapped.message);
        assert!(mapped.message.contains("<upstream>"), "{}", mapped.message);
        drop(listener);
    }

    /// Чем кончается сценарный апстрим после своих кусков.
    enum ScriptEnd {
        Close,
        Fail(fn() -> AppError),
        /// Держит канал открытым, пока тест не отпустит.
        HoldUntil(Arc<tokio::sync::Notify>),
    }

    /// Апстрим по сценарию: куски `(текст, причина конца)`, затем `end`.
    struct Scripted {
        chunks: Vec<(&'static str, Option<&'static str>)>,
        end: ScriptEnd,
    }

    #[async_trait::async_trait]
    impl LLMProvider for Scripted {
        async fn complete(&self, _request: CompletionRequest) -> AppResult<CompletionResponse> {
            unreachable!("the bridge streams")
        }

        async fn complete_stream(
            &self,
            _request: CompletionRequest,
            tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
        ) -> AppResult<()> {
            for (content, finish) in &self.chunks {
                let _ = tx.send(CompletionChunk {
                    content: content.to_string(),
                    tool_calls: Vec::new(),
                    finish_reason: finish.map(str::to_string),
                    usage: None,
                });
            }
            match &self.end {
                ScriptEnd::Close => Ok(()),
                ScriptEnd::Fail(error) => Err(error()),
                ScriptEnd::HoldUntil(release) => {
                    release.notified().await;
                    Ok(())
                }
            }
        }

        fn model(&self) -> &str {
            "scripted"
        }

        fn fork(&self) -> Arc<dyn LLMProvider> {
            unreachable!("the bridge never forks")
        }
    }

    /// Чат-мост над сценарием; кадры читаются по мере прихода.
    fn start_bridge(provider: Scripted) -> tokio::sync::mpsc::UnboundedReceiver<Bytes> {
        let (body_tx, body_rx) = tokio::sync::mpsc::unbounded_channel();
        let request = CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: Vec::new(),
            model: "scripted".to_string(),
            temperature: 0.0,
            max_tokens: 64,
            stream: true,
        };
        let meta = crate::provider::openai_compat::CompletionMeta::generate("m");
        tokio::spawn(super::chat_bridge(
            super::Upstream::start(Arc::new(provider), request),
            super::Pump::none(),
            meta,
            body_tx,
        ));
        body_rx
    }

    /// Следующий кадр; `None` — мост закончил.
    async fn next_frame(
        body_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Bytes>,
    ) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(5), body_rx.recv())
            .await
            .expect("the bridge stalled")
            .map(|frame| String::from_utf8(frame.to_vec()).expect("utf-8 frame"))
    }

    async fn all_frames(mut body_rx: tokio::sync::mpsc::UnboundedReceiver<Bytes>) -> Vec<String> {
        let mut frames = Vec::new();
        while let Some(frame) = next_frame(&mut body_rx).await {
            frames.push(frame);
        }
        frames
    }

    fn frame_json(frame: &str) -> serde_json::Value {
        let payload = frame
            .strip_prefix("data: ")
            .expect("a data frame")
            .trim_end();
        serde_json::from_str(payload).expect("frame json")
    }

    /// Весь ответ удержан сплиттером, апстрим не закрывает канал: хвост и
    /// финал всё равно уходят сразу после причины конца, а не по простою.
    #[tokio::test]
    async fn a_finish_with_a_held_back_tail_flushes_it_and_ends_the_stream() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut body_rx = start_bridge(Scripted {
            chunks: vec![("<thi", Some("stop"))],
            end: ScriptEnd::HoldUntil(Arc::clone(&release)),
        });
        let tail = next_frame(&mut body_rx).await.expect("the tail frame");
        assert_eq!(frame_json(&tail)["choices"][0]["delta"]["content"], "<thi");
        release.notify_one();
        let rest = all_frames(body_rx).await;
        assert_eq!(rest.len(), 2, "{rest:?}");
        assert_eq!(frame_json(&rest[0])["choices"][0]["finish_reason"], "stop");
        assert_eq!(rest[1], "data: [DONE]\n\n");
    }

    /// Ошибка после причины конца не портит уже целый ответ.
    #[tokio::test]
    async fn an_error_after_the_finish_still_ends_with_done() {
        let frames = all_frames(start_bridge(Scripted {
            chunks: vec![("Hi", Some("stop"))],
            end: ScriptEnd::Fail(|| AppError::Provider("connection reset".to_string())),
        }))
        .await;
        assert_eq!(
            frame_json(&frames[0])["choices"][0]["delta"]["content"],
            "Hi"
        );
        assert_eq!(frames.last().map(String::as_str), Some("data: [DONE]\n\n"));
        assert!(frames.iter().all(|frame| !frame.contains("\"error\"")));
    }

    #[tokio::test]
    async fn a_stream_closed_without_a_finish_ends_with_an_error_frame() {
        let frames = all_frames(start_bridge(Scripted {
            chunks: vec![("Hi", None)],
            end: ScriptEnd::Close,
        }))
        .await;
        let last = frame_json(frames.last().expect("frames"));
        assert_eq!(last["error"]["type"], "server_error");
        let message = last["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("before finishing"), "{message}");
        assert!(frames.iter().all(|frame| !frame.contains("[DONE]")));
    }

    /// Лимит посреди стрима — кадр `rate_limit_error`: клиент поймёт, что ждать.
    #[tokio::test]
    async fn a_rate_limit_mid_stream_is_a_rate_limit_error_frame() {
        let frames = all_frames(start_bridge(Scripted {
            chunks: Vec::new(),
            end: ScriptEnd::Fail(|| AppError::RateLimited("slow down".to_string())),
        }))
        .await;
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frame_json(&frames[0])["error"]["type"], "rate_limit_error");
    }
}
