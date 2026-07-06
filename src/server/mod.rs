//! The local API server (`/serve on`, `--serve`): pooprusteek as an
//! OpenAI-compatible gateway to every provider it knows — the built-in
//! DeepSeek web client plus all `/providers` entries — so OpenAI-speaking
//! tools (aider, Continue, OpenWebUI, …) can point at
//! `http://127.0.0.1:<port>/v1` and pick a backend by model id.
//!
//! Layout mirrors the layering elsewhere in the codebase:
//! - `catalog` — pure model-id → backend resolution (`deepseek-chat`,
//!   `entry/model`, …), no I/O;
//! - `http` — hyper transport: accept loop, auth, CORS, dialect dispatch;
//! - `openai` — the OpenAI Chat Completions dialect (the only one
//!   implemented today; `config::ServerApi` reserves Anthropic/Gemini).
//!
//! Session strategy is stateless-per-request (like the APIs it imitates):
//! every completion runs on a fresh `LLMProvider::fork()`, and DeepSeek
//! forks discard their remote session afterwards so server traffic never
//! piles junk chats onto the user's account.
//!
//! The server runs as a detached tokio task owned by [`ServerHandle`] and
//! reports lifecycle through `AppEvent`s (`ServerStarted` / `ServerFailed`
//! / `ServerStopped`), each tagged with a generation so a restarted
//! server's events can't be mistaken for its predecessor's.

pub mod catalog;
mod http;
mod openai;
pub mod proxy;

use crate::app::events::AppEvent;
use crate::config::{Config, ProviderConfig, ProviderEntry, ServerApi};
use crate::provider::model_cache::ProviderModelCache;
use crate::provider::openai_compat::RequestDefaults;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

/// Everything the server task needs, snapshotted from `Config` at launch.
/// Provider changes made in the TUI while the server runs apply on the next
/// `/serve off` + `/serve on` (or `/server <port>` restart).
#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub api: ServerApi,
    /// Required bearer token, when auth is enabled.
    pub api_key: Option<String>,
    /// Fill-ins for temperature/max_tokens the wire request may omit.
    pub defaults: RequestDefaults,
    /// Seed for the built-in DeepSeek backend, when a token is configured.
    pub deepseek: Option<DeepseekSeed>,
    pub entries: Vec<ProviderEntry>,
    /// Emit an `AppEvent::ServerRequestLog` per request. On for proxy mode
    /// (the log IS the UI there); off for the TUI, which shows counters.
    pub request_log: bool,
}

/// What `DeepseekProvider::new` needs, detached from the live `Config`.
#[derive(Debug, Clone)]
pub struct DeepseekSeed {
    pub provider: ProviderConfig,
    pub rate_limit_ms: u64,
    pub rate_limit_per_minute: u32,
    pub max_retries: i32,
}

impl ServerSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            host: config.server.host.clone(),
            port: config.server.port,
            api: config.server.api,
            api_key: config
                .server
                .api_key
                .clone()
                .filter(|key| !key.trim().is_empty()),
            defaults: RequestDefaults {
                temperature: config.provider.temperature,
                max_tokens: config.provider.max_tokens,
            },
            deepseek: (!config.provider.token.is_empty()).then(|| DeepseekSeed {
                provider: config.provider.clone(),
                rate_limit_ms: config.agent.rate_limit_ms,
                rate_limit_per_minute: config.agent.rate_limit_per_minute,
                max_retries: config.agent.max_retries,
            }),
            entries: config.providers.clone(),
            request_log: false,
        }
    }
}

/// Request counters shared between the server task and the `/serve` status
/// display. Plain atomics — read synchronously on the event loop.
#[derive(Debug, Default)]
pub struct ServerStats {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
}

/// The app's grip on a running (or starting) server task.
pub struct ServerHandle {
    /// Monotonic launch id; lifecycle `AppEvent`s echo it so events from a
    /// replaced server can't clobber the current one.
    pub generation: u64,
    pub host: String,
    pub port: u16,
    pub api: ServerApi,
    /// Confirmed listen address, set when `ServerStarted` arrives (`None`
    /// while binding).
    pub bound_addr: Option<std::net::SocketAddr>,
    pub started_at: std::time::Instant,
    pub stats: Arc<ServerStats>,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    /// Ask the accept loop to exit. The task confirms with `ServerStopped`.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Hard-kill the task (app exit — no event listener left to notify).
    pub fn abort(&self) {
        self.task.abort();
    }
}

/// Launch the server task. Binding happens inside the task — never on the
/// event loop — and the outcome comes back as `ServerStarted`/`ServerFailed`.
/// `models` is the shared provider-model cache: the app (or proxy) keeps it
/// fresh; the server reads it live, so `/v1/models` grows as fetches land.
pub fn spawn(
    settings: ServerSettings,
    models: Arc<ProviderModelCache>,
    generation: u64,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> ServerHandle {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let stats = Arc::new(ServerStats::default());
    let handle_stats = Arc::clone(&stats);
    let (host, port, api) = (settings.host.clone(), settings.port, settings.api);
    let task = tokio::spawn(http::run(settings, models, generation, event_tx, shutdown_rx, stats));
    ServerHandle {
        generation,
        host,
        port,
        api,
        bound_addr: None,
        started_at: std::time::Instant::now(),
        stats: handle_stats,
        shutdown: shutdown_tx,
        task,
    }
}
