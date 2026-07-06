//! Hyper transport for the API server: bind + accept loop, per-connection
//! service, bearer auth, CORS, and dispatch into the configured wire
//! dialect. No wire-format knowledge lives here — that's `openai.rs` (and
//! future dialect modules).

use super::{catalog, openai, ServerSettings, ServerStats};
use crate::app::events::AppEvent;
use crate::config::{ProviderEntry, ServerApi};
use crate::provider::model_cache::ProviderModelCache;
use crate::provider::openai_compat::{ErrorResponse, RequestDefaults};
use crate::provider::LLMProvider;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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
    pub api_key: Option<String>,
    pub defaults: RequestDefaults,
    /// Base DeepSeek client — `fork()`ed per request — when a token is
    /// configured. Built once so all server traffic shares one rate limiter.
    pub deepseek: Option<Arc<dyn LLMProvider>>,
    pub entries: Vec<ProviderEntry>,
    /// Live fetched-model view (shared with the app/proxy refresher) —
    /// `/v1/models` and routing pick up new fetches without a restart.
    pub models: Arc<ProviderModelCache>,
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
        Self {
            api: settings.api,
            api_key: settings.api_key,
            defaults: settings.defaults,
            deepseek,
            entries: settings.entries,
            models,
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
    tracing::info!(
        "server: listening on {} ({} dialect)",
        bound.map(|a| a.to_string()).unwrap_or_else(|| address.clone()),
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
    // Permissive CORS so browser-based OpenAI clients can call a local
    // gateway; the bearer check (when enabled) remains the actual gate.
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().expect("static header value"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, OPTIONS".parse().expect("static header value"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "authorization, content-type".parse().expect("static header value"),
    );
    Ok(response)
}

async fn route(request: Request<Incoming>, context: &Arc<ServerContext>) -> Response<ApiBody> {
    if request.method() == Method::OPTIONS {
        return empty_response(StatusCode::NO_CONTENT);
    }

    let path = request.uri().path().to_string();
    if request.method() == Method::GET && (path == "/" || path == "/health") {
        return json_response(
            StatusCode::OK,
            &serde_json::json!({
                "status": "ok",
                "server": "pooprusteek",
                "api": context.api.label(),
                "models": catalog::list_model_ids(
                    context.deepseek.is_some(),
                    &context.entries,
                    &context.models.snapshot(),
                ),
            }),
        );
    }

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

    context.stats.requests.fetch_add(1, Ordering::Relaxed);

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
    use crate::server::{spawn, ServerSettings};

    /// Bindable-anywhere settings: port 0 (OS-assigned), no providers.
    fn test_settings(api_key: Option<&str>) -> ServerSettings {
        ServerSettings {
            host: "127.0.0.1".to_string(),
            port: 0,
            api: crate::config::ServerApi::Openai,
            api_key: api_key.map(str::to_string),
            defaults: RequestDefaults { temperature: 0.7, max_tokens: 4096 },
            deepseek: None,
            entries: Vec::new(),
            request_log: false,
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
            Some(AppEvent::ServerStarted { generation: 7, addr }) => addr,
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
}
