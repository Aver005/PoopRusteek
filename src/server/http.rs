//! Hyper transport for the API server: bind + accept loop, per-connection
//! service, bearer auth, CORS, and dispatch into the configured wire
//! dialect. No wire-format knowledge lives here — that's `openai.rs` (and
//! future dialect modules).

use super::{ServerSettings, ServerStats, catalog, openai};
use crate::app::events::AppEvent;
use crate::config::{ProviderEntry, ServerApi};
use crate::provider::LLMProvider;
use crate::provider::model_cache::ProviderModelCache;
use crate::provider::openai_compat::{ErrorResponse, RequestDefaults};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::{mpsc, watch};

/// All response bodies are boxed so fixed JSON replies and SSE streams
/// share one service signature.
pub(super) type ApiBody = BoxBody<Bytes, std::convert::Infallible>;

/// Attached to a response by a dialect handler to enrich the access-log
/// line (model id, streaming flag) without widening any signatures.
#[derive(Debug, Clone)]
pub(super) struct LogDetail(pub String);

/// Per-launch state shared by every connection.
pub(super) struct ServerContext {
    pub api: ServerApi,
    pub host: String,
    /// Имена сверх loopback, разрешённые в `Host` (защита от DNS rebinding).
    pub allowed_hosts: Vec<String>,
    /// Порт, на котором сервер действительно слушает (известен после привязки).
    pub bound_port: std::sync::atomic::AtomicU16,
    pub api_key: Option<String>,
    pub defaults: RequestDefaults,
    /// Base DeepSeek client — `fork()`ed per request — when a token is
    /// configured. Built once so all server traffic shares one rate limiter.
    pub deepseek: Option<Arc<dyn LLMProvider>>,
    pub entries: Vec<ProviderEntry>,
    /// Live fetched-model view (shared with the app/proxy refresher) —
    /// `/v1/models` and routing pick up new fetches without a restart.
    pub models: Arc<ProviderModelCache>,
    /// Reused client for endpoints proxied verbatim to an upstream
    /// (`/embeddings`, `/rerank`) — see `openai::passthrough`.
    pub http_client: reqwest::Client,
    /// Живой шлюз: включённый ведёт все запросы, выключенный оставляет прежний путь.
    pub gateway: super::GatewayCell,
    pub stats: Arc<ServerStats>,
    /// `Some` when per-request access-log events are wanted (proxy mode).
    pub log_tx: Option<mpsc::UnboundedSender<AppEvent>>,
}

impl ServerContext {
    fn build(
        settings: ServerSettings,
        models: Arc<ProviderModelCache>,
        stats: Arc<ServerStats>,
        log_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    ) -> Self {
        let deepseek = settings.deepseek.as_ref().and_then(|seed| {
            match crate::provider::deepseek::DeepseekProvider::new(
                &seed.provider,
                seed.rate_limit_ms,
                seed.rate_limit_per_minute,
                seed.max_retries,
            ) {
                Ok(provider) => Some(Arc::new(provider) as Arc<dyn LLMProvider>),
                Err(error) => {
                    tracing::warn!("server: DeepSeek backend unavailable: {error}");
                    None
                }
            }
        });
        // Passthrough calls (embeddings/rerank) can be slow on big batches;
        // give them a generous read timeout, fail fast on dead endpoints.
        let http_client = reqwest::Client::builder()
            // Ключ записи уходит заголовком: за редиректом на другой хост не идём.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            // Пакет эмбеддингов на CPU бывает долгим: срок щедрее, чем у чата.
            .read_timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self {
            api: settings.api,
            host: settings.host,
            allowed_hosts: settings.allowed_hosts,
            bound_port: std::sync::atomic::AtomicU16::new(settings.port),
            api_key: settings.api_key,
            defaults: settings.defaults,
            deepseek,
            entries: settings.entries,
            models,
            http_client,
            gateway: settings.gateway,
            stats,
            log_tx,
        }
    }
}

/// The server task: bind (with brief retries so a `/server <port>` restart
/// can win the race against its predecessor's closing listener), report
/// via `AppEvent`, then accept until shutdown.
pub(super) async fn run(
    settings: ServerSettings,
    models: Arc<ProviderModelCache>,
    generation: u64,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
    stats: Arc<ServerStats>,
) {
    let address = format!("{}:{}", settings.host, settings.port);
    let log_tx = settings.request_log.then(|| event_tx.clone());
    let context = Arc::new(ServerContext::build(settings, models, stats, log_tx));

    let mut listener = None;
    let mut last_error = String::new();
    for attempt in 0..10 {
        match tokio::net::TcpListener::bind(&address).await {
            Ok(bound) => {
                listener = Some(bound);
                break;
            }
            Err(error) => {
                last_error = error.to_string();
                if attempt < 9 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
    let Some(listener) = listener else {
        tracing::warn!("server: failed to bind {address}: {last_error}");
        let _ = event_tx.send(AppEvent::ServerFailed {
            generation,
            error: format!("could not bind {address}: {last_error}"),
        });
        return;
    };

    // `/serve off` may have landed while we were still binding.
    if *shutdown_rx.borrow() {
        let _ = event_tx.send(AppEvent::ServerStopped { generation });
        return;
    }

    let bound = listener.local_addr().ok();
    if let Some(addr) = bound {
        context.bound_port.store(addr.port(), Ordering::Relaxed);
    }
    tracing::info!(
        "server: listening on {} ({} dialect)",
        bound
            .map(|a| a.to_string())
            .unwrap_or_else(|| address.clone()),
        context.api.label()
    );
    if let Some(addr) = bound {
        let _ = event_tx.send(AppEvent::ServerStarted { generation, addr });
    }

    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            // Also fires on Err when the handle was dropped — either way, stop.
            _ = shutdown_rx.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    let context = Arc::clone(&context);
                    connections.spawn(async move {
                        let service = service_fn(move |request| {
                            handle_request(request, Arc::clone(&context))
                        });
                        if let Err(error) = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                        {
                            tracing::debug!("server: connection ended with error: {error}");
                        }
                    });
                }
                Err(error) => tracing::warn!("server: accept failed: {error}"),
            },
            // Reap finished connection tasks so the set doesn't grow forever.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }

    connections.abort_all();
    tracing::info!("server: stopped");
    let _ = event_tx.send(AppEvent::ServerStopped { generation });
}

async fn handle_request(
    request: Request<Incoming>,
    context: Arc<ServerContext>,
) -> Result<Response<ApiBody>, std::convert::Infallible> {
    let started = std::time::Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    // Чужая страница CORS-заголовков не получает вовсе: ответ ей читать нельзя.
    let page_origin = request.headers().get(header::ORIGIN).cloned();
    let foreign_page = page_origin
        .as_ref()
        .is_some_and(|origin| !origin_is_allowed(&context, origin));
    let mut response = route(request, &context).await;
    if response.status().is_client_error() || response.status().is_server_error() {
        context.stats.errors.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(log_tx) = &context.log_tx {
        let detail = response
            .extensions()
            .get::<LogDetail>()
            .map(|detail| format!(" · {}", detail.0))
            .unwrap_or_default();
        let _ = log_tx.send(AppEvent::ServerRequestLog {
            line: format!(
                "{method} {path} → {} ({}ms){detail}",
                response.status().as_u16(),
                started.elapsed().as_millis()
            ),
        });
    }
    // CORS: с ключом — для всех; без ключа — только странице, чей `Origin` уже прошёл проверку
    // (своя или из `allowed_hosts`). Иначе единственный барьер чтения ответа — политика браузера.
    if !foreign_page && (context.api_key.is_some() || page_origin.is_some()) {
        let headers = response.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            page_origin
                .clone()
                .unwrap_or_else(|| "*".parse().expect("static header value")),
        );
        headers.append(header::VARY, "Origin".parse().expect("static header value"));
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, OPTIONS".parse().expect("static header value"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "authorization, content-type"
                .parse()
                .expect("static header value"),
        );
    }
    Ok(response)
}

async fn route(request: Request<Incoming>, context: &Arc<ServerContext>) -> Response<ApiBody> {
    // Сервер на loopback отвечает только тем, кто назвал себя loopback-именем: иначе страница
    // чужого сайта после DNS rebinding читала бы ответы как свои.
    if super::is_loopback_host(&context.host) {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        let port = context.bound_port.load(Ordering::Relaxed);
        if !super::routing::host_allowed(host, port, &context.allowed_hosts) {
            return json_response(
                StatusCode::FORBIDDEN,
                &ErrorResponse::invalid_request(
                    "unexpected Host header; add the name to [server] allowed_hosts to use it",
                ),
            );
        }
    }

    // Страница из браузера, чужая серверу, не пускается даже с верным ключом: простой кросс-доменный
    // POST исполнился бы, даже если ответ ей недоступен (CSRF). Клиенты SDK `Origin` не шлют.
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && !origin_is_allowed(context, origin)
    {
        return json_response(
            StatusCode::FORBIDDEN,
            &ErrorResponse::invalid_request(
                "requests from other web pages are refused; add the page's host to [server] allowed_hosts to allow it",
            ),
        );
    }

    if request.method() == Method::OPTIONS {
        return empty_response(StatusCode::NO_CONTENT);
    }

    let path = request.uri().path().to_string();
    if request.method() == Method::GET && (path == "/" || path == "/health") {
        // Открытый путь: за шлюзом имена аккаунтов наружу без ключа не идут.
        let models = if context.gateway.get().is_enabled() {
            Vec::new()
        } else {
            catalog::list_model_ids(
                context.deepseek.is_some(),
                &context.entries,
                &context.models.snapshot(),
            )
        };
        return json_response(
            StatusCode::OK,
            &serde_json::json!({
                "status": "ok",
                "server": "pooprusteek",
                "api": context.api.label(),
                "models": models,
            }),
        );
    }

    // Count the request before the auth gate — `handle_request` increments
    // `errors` for any 4xx, so a 401 without a matching `requests` increment
    // skews the error rate (errors > requests under a misconfigured client).
    context.stats.requests.fetch_add(1, Ordering::Relaxed);

    if let Some(expected) = &context.api_key {
        let authorized = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| token.trim() == expected);
        if !authorized {
            return json_response(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse::invalid_request(
                    "missing or invalid bearer token (Authorization: Bearer …)",
                ),
            );
        }
    }

    // Тело POST — только JSON: `text/plain` и формы шлёт «простой» запрос без предзапроса.
    if request.method() == Method::POST && !is_json_content_type(request.headers()) {
        return json_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &ErrorResponse::invalid_request(
                "POST bodies must be sent as Content-Type: application/json",
            ),
        );
    }

    if request.method() == Method::GET
        && matches!(
            path.as_str(),
            "/pooprusteek/status" | "/v1/pooprusteek/status"
        )
    {
        return status_response(context);
    }

    let handled = match context.api {
        ServerApi::Openai => openai::route(request, context).await,
        // Reserved dialects: the config parses, the seam exists, the
        // inbound conversions don't yet (PLANS.md).
        ServerApi::Anthropic | ServerApi::Gemini => Some(json_response(
            StatusCode::NOT_IMPLEMENTED,
            &ErrorResponse::server_error(format!(
                "the '{}' API dialect is not implemented yet — set [server] api = \"openai\"",
                context.api.label()
            )),
        )),
    };

    handled.unwrap_or_else(|| {
        json_response(
            StatusCode::NOT_FOUND,
            &ErrorResponse::invalid_request(format!("no route for {path}")),
        )
    })
}

/// Страница с таким `Origin` своя серверу или названа в `allowed_hosts`.
fn origin_is_allowed(context: &ServerContext, origin: &header::HeaderValue) -> bool {
    let port = context.bound_port.load(Ordering::Relaxed);
    origin.to_str().is_ok_and(|origin| {
        super::routing::origin_allowed(origin, port, &context.host, &context.allowed_hosts)
    })
}

/// `Content-Type: application/json` (параметры вроде `charset` допустимы, регистр не важен).
fn is_json_content_type(headers: &hyper::HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// Статус шлюза. Ключ сервера уже проверен выше; без ключа отдаётся урезанный вид и только
/// при привязке к loopback: иначе чужой в сети увидел бы состояние чужих подписок.
fn status_response(context: &ServerContext) -> Response<ApiBody> {
    let gateway = context.gateway.get();
    if !gateway.is_enabled() {
        return json_response(
            StatusCode::NOT_FOUND,
            &ErrorResponse::invalid_request("the gateway is disabled: nothing to report"),
        );
    }
    use super::routing::{StatusAccess, status_access};
    let access = status_access(context.api_key.is_some(), &context.host);
    if access == StatusAccess::Forbidden {
        return json_response(
            StatusCode::FORBIDDEN,
            &ErrorResponse::invalid_request(
                "set [server] api_key to read the gateway status beyond loopback",
            ),
        );
    }
    json_response(
        StatusCode::OK,
        &super::routing::status_body(&gateway.snapshot(), access == StatusAccess::Detailed),
    )
}

// ── Response helpers (shared with the dialect modules) ─────────────────

pub(super) fn full_body(bytes: impl Into<Bytes>) -> ApiBody {
    Full::new(bytes.into()).boxed()
}

pub(super) fn empty_response(status: StatusCode) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .body(full_body(Bytes::new()))
        .expect("static response parts are valid")
}

pub(super) fn json_response(status: StatusCode, payload: &impl Serialize) -> Response<ApiBody> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(body))
        .expect("static response parts are valid")
}

#[cfg(test)]
mod tests {
    use crate::app::events::AppEvent;
    use crate::provider::openai_compat::RequestDefaults;
    use crate::server::{ServerSettings, spawn};

    /// Bindable-anywhere settings: port 0 (OS-assigned), no providers.
    fn test_settings(api_key: Option<&str>) -> ServerSettings {
        ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 0,
            api: crate::config::ServerApi::Openai,
            api_key: api_key.map(str::to_string),
            defaults: RequestDefaults {
                temperature: 0.7,
                max_tokens: 4096,
            },
            deepseek: None,
            entries: Vec::new(),
            request_log: false,
            allowed_hosts: Vec::new(),
            gateway: Default::default(),
        }
    }

    /// End-to-end transport smoke test over a real socket: lifecycle
    /// events, routing, auth, error envelopes, shutdown. Provider-backed
    /// completion paths are covered by the catalog/wire-format unit tests —
    /// no network leaves the loopback here (no providers are configured).
    #[tokio::test]
    async fn server_serves_routes_and_shuts_down() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
        let handle = spawn(test_settings(Some("sekret")), models, 7, event_tx);

        // No `{:?}` on the mismatch: Debug-formatting an `AppEvent` here
        // would mark `AgentResult`'s fields as read and unfulfill their
        // `#[expect(dead_code)]` in test builds.
        let addr = match event_rx.recv().await {
            Some(AppEvent::ServerStarted {
                generation: 7,
                addr,
            }) => addr,
            _ => panic!("expected ServerStarted for generation 7"),
        };
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // /health is open (no auth) and reports the dialect.
        let health: serde_json::Value = client
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health request")
            .json()
            .await
            .expect("health json");
        assert_eq!(health["status"], "ok");
        assert_eq!(health["api"], "openai");

        // API routes require the bearer key when one is configured.
        let unauthorized = client
            .get(format!("{base}/v1/models"))
            .send()
            .await
            .expect("models request");
        assert_eq!(unauthorized.status(), 401);

        let models = client
            .get(format!("{base}/v1/models"))
            .bearer_auth("sekret")
            .send()
            .await
            .expect("models request");
        assert_eq!(models.status(), 200);
        let models: serde_json::Value = models.json().await.expect("models json");
        assert_eq!(models["object"], "list");
        assert_eq!(models["data"].as_array().map(Vec::len), Some(0));

        // Unknown model → OpenAI error envelope with model_not_found.
        let not_found = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("sekret")
            .json(&serde_json::json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .send()
            .await
            .expect("completion request");
        assert_eq!(not_found.status(), 404);
        let envelope: serde_json::Value = not_found.json().await.expect("error json");
        assert_eq!(envelope["error"]["code"], "model_not_found");

        // Malformed body → 400, unknown path → 404.
        let bad = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("sekret")
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .expect("bad-body request");
        assert_eq!(bad.status(), 400);
        let lost = client
            .get(format!("{base}/v1/telemetry"))
            .bearer_auth("sekret")
            .send()
            .await
            .expect("unknown-path request");
        assert_eq!(lost.status(), 404);

        // Graceful shutdown confirms with a generation-tagged event.
        handle.request_shutdown();
        loop {
            match event_rx.recv().await {
                Some(AppEvent::ServerStopped { generation: 7 }) => break,
                Some(_) => continue,
                None => panic!("server task ended without ServerStopped"),
            }
        }
    }

    /// CORS headers are the actual cross-origin gate when no bearer token is
    /// configured — they must not be sent, or any page could read replies.
    #[tokio::test]
    async fn cors_headers_follow_api_key_presence() {
        async fn cors_headers_present(api_key: Option<&str>) -> bool {
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
            let handle = spawn(test_settings(api_key), models, 1, event_tx);
            let addr = match event_rx.recv().await {
                Some(AppEvent::ServerStarted { addr, .. }) => addr,
                _ => panic!("expected ServerStarted"),
            };
            let base = format!("http://{addr}");
            let client = reqwest::Client::new();

            // /health (open route) and the OPTIONS preflight must agree.
            let health = client
                .get(format!("{base}/health"))
                .send()
                .await
                .expect("health request");
            let preflight = client
                .request(
                    reqwest::Method::OPTIONS,
                    format!("{base}/v1/chat/completions"),
                )
                .send()
                .await
                .expect("preflight request");
            let present = health
                .headers()
                .contains_key(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN);
            assert_eq!(
                present,
                preflight
                    .headers()
                    .contains_key(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN),
                "health and preflight must agree on CORS presence"
            );

            handle.request_shutdown();
            while let Some(event) = event_rx.recv().await {
                if matches!(event, AppEvent::ServerStopped { .. }) {
                    break;
                }
            }
            present
        }

        assert!(
            !cors_headers_present(None).await,
            "no api_key configured → no CORS headers (browser same-origin policy stays the gate)"
        );
        assert!(
            cors_headers_present(Some("sekret")).await,
            "api_key configured → CORS headers present (bearer check is the real gate)"
        );
    }

    /// Поднять сервер на свободном порту и вернуть адрес вместе с ручкой остановки.
    async fn start(
        settings: ServerSettings,
    ) -> (
        String,
        crate::server::ServerHandle,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
        let handle = spawn(settings, models, 1, event_tx);
        let addr = match event_rx.recv().await {
            Some(AppEvent::ServerStarted { addr, .. }) => addr,
            _ => panic!("expected ServerStarted"),
        };
        (addr.to_string(), handle, event_rx)
    }

    fn chat_body() -> serde_json::Value {
        serde_json::json!({"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]})
    }

    /// Простой кросс-доменный POST (`text/plain`) исполнился бы без предзапроса: сервер отказывает.
    #[tokio::test]
    async fn a_post_that_is_not_json_is_refused() {
        let (addr, handle, _events) = start(test_settings(None)).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/chat/completions");
        for content_type in [
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            None,
        ] {
            let mut request = client.post(&url).body(chat_body().to_string());
            if let Some(value) = content_type {
                request = request.header("content-type", value);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                415,
                "{content_type:?}"
            );
        }
        // Обычный клиент SDK: JSON, в том числе с параметром и в другом регистре.
        for value in ["application/json", "Application/JSON; charset=utf-8"] {
            let reply = client
                .post(&url)
                .header("content-type", value)
                .body(chat_body().to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(
                reply.status(),
                404,
                "unknown model reaches the handler: {value}"
            );
        }
        // Чтение (GET) типа тела не требует.
        let models = client
            .get(format!("http://{addr}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(models.status(), 200);
        handle.request_shutdown();
    }

    /// Страница с чужим `Origin` отклоняется, даже если у неё есть верный ключ.
    #[tokio::test]
    async fn a_foreign_origin_is_refused_even_with_the_key() {
        for api_key in [None, Some("sekret")] {
            let (addr, handle, _events) = start(test_settings(api_key)).await;
            let client = reqwest::Client::new();
            let url = format!("http://{addr}/v1/chat/completions");
            for origin in [
                "https://evil.example",
                "null",
                "http://127.0.0.1:1",
                "http://localhost",
            ] {
                let mut request = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("origin", origin)
                    .body(chat_body().to_string());
                if let Some(key) = api_key {
                    request = request.bearer_auth(key);
                }
                let reply = request.send().await.unwrap();
                assert_eq!(reply.status(), 403, "{origin} with key {api_key:?}");
                assert!(
                    !reply.headers().contains_key("access-control-allow-origin"),
                    "a refused page must not be allowed to read the reply"
                );
            }
            let preflight = client
                .request(reqwest::Method::OPTIONS, &url)
                .header("origin", "https://evil.example")
                .send()
                .await
                .unwrap();
            assert_eq!(preflight.status(), 403);
            handle.request_shutdown();
        }
    }

    /// Своя страница сервера и имена из `allowed_hosts` проходят; клиенты без `Origin` не задеты.
    #[tokio::test]
    async fn own_pages_allowed_hosts_and_sdk_clients_pass() {
        let mut settings = test_settings(None);
        settings.allowed_hosts = vec!["webui.example".to_string()];
        let (addr, handle, _events) = start(settings).await;
        let port = addr.rsplit(':').next().unwrap().to_string();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/chat/completions");
        for origin in [
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
            "https://webui.example".to_string(),
            "https://WebUI.example:8443".to_string(),
        ] {
            let reply = client
                .post(&url)
                .header("content-type", "application/json")
                .header("origin", &origin)
                .body(chat_body().to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), 404, "{origin} reaches the handler");
            assert_eq!(
                reply
                    .headers()
                    .get("access-control-allow-origin")
                    .map(|v| v.to_str().unwrap()),
                Some(origin.as_str()),
                "an allowed page reads the reply"
            );
        }
        // SDK: без Origin, JSON; `/v1/embeddings` доходит до обработчика (нет провайдера → 404, не 415/403).
        let embeddings = client
            .post(format!("http://{addr}/v1/embeddings"))
            .header("content-type", "application/json")
            .body(serde_json::json!({"model": "text-embedding-3-small", "input": "hi"}).to_string())
            .send()
            .await
            .unwrap();
        assert!(
            ![403u16, 415].contains(&embeddings.status().as_u16()),
            "{}",
            embeddings.status()
        );
        handle.request_shutdown();
    }
}
