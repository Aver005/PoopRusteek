use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    /// The compaction ladder — see `crate::config::ContextConfig`.
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Semantic (embedding-based) skill matching — see `crate::semantic`.
    #[serde(default)]
    pub semantic: SemanticConfig,
    /// Additional OpenAI-compatible providers (LM Studio, Ollama, vLLM,
    /// OpenRouter, …) managed via `/providers`. The DeepSeek web client
    /// configured by `[provider]` above is the implicit built-in entry and
    /// never appears in this list.
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    /// Name of the entry in `providers` that is currently active. `None`
    /// (or a name that no longer exists) means the built-in DeepSeek
    /// provider.
    #[serde(default)]
    pub active_provider: Option<String>,
    /// The local API server (`--serve` / `/serve`) — see `crate::server`.
    #[serde(default)]
    pub server: ServerConfig,
    /// Background fetching of `/providers` model lists — see
    /// `crate::provider::model_cache`.
    #[serde(default)]
    pub provider_models: ProviderModelsConfig,
    /// Self-update from the rolling `latest` release — see `crate::update`.
    #[serde(default)]
    pub update: UpdateConfig,
}

/// `[update]` — the self-updater (`/update`, `/autoupdate`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// `/autoupdate on|off` — when true, every TUI startup checks the
    /// `latest` release in the background and installs it on hash mismatch
    /// (takes effect on the next launch). Off by default: replacing the
    /// binary behind the user's back must be an explicit opt-in.
    #[serde(default = "default_update_auto")]
    pub auto: bool,
}

fn default_update_auto() -> bool {
    false
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            auto: default_update_auto(),
        }
    }
}

/// `[provider_models]` — how the per-provider model catalogs (fetched via
/// each entry's `GET /models`) are kept fresh. They feed the API server's
/// `/v1/models` and model-id routing, plus the `/providers` panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModelsConfig {
    /// Background refetch period in ms (`/refetch-providers`); 0 = off
    /// (fetch only on startup and when a provider is added).
    #[serde(default = "default_provider_models_ms")]
    pub refetch_ms: u64,
    /// How long a persisted model list stays valid across restarts in ms
    /// (`/cache-providers`); 0 = off (every startup re-fetches).
    #[serde(default = "default_provider_models_ms")]
    pub cache_ms: u64,
}

fn default_provider_models_ms() -> u64 {
    ProviderModelsConfig::DEFAULT_MS
}

impl Default for ProviderModelsConfig {
    fn default() -> Self {
        Self {
            refetch_ms: default_provider_models_ms(),
            cache_ms: default_provider_models_ms(),
        }
    }
}

impl ProviderModelsConfig {
    /// Both knobs default to 3 minutes.
    pub const DEFAULT_MS: u64 = 180_000;
}

/// `[server]` — the local HTTP API gateway (`/serve on`, `--serve`).
/// Exposes every configured provider (built-in DeepSeek + `/providers`
/// entries) behind one endpoint speaking `api`'s wire dialect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address. Loopback by default — exposing a keyless gateway to
    /// paid/authed upstreams on a LAN must be an explicit decision.
    #[serde(default = "default_server_host")]
    pub host: String,
    /// Listen port, persisted by `/server <port>`.
    #[serde(default = "default_server_port")]
    pub port: u16,
    /// Which wire dialect the server speaks — see [`ServerApi`].
    #[serde(default)]
    pub api: ServerApi,
    /// When set, every request must carry `Authorization: Bearer <this>`.
    /// `None`/empty = no auth (fine on loopback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

fn default_server_host() -> String {
    "127.0.0.1".to_string()
}

fn default_server_port() -> u16 {
    ServerConfig::DEFAULT_PORT
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_server_host(),
            port: default_server_port(),
            api: ServerApi::default(),
            api_key: None,
        }
    }
}

impl ServerConfig {
    /// "poop" on a T9 keypad — memorable and outside every well-known
    /// local-LLM port (ollama 11434, LM Studio 1234, llama.cpp 8080).
    pub const DEFAULT_PORT: u16 = 7667;
}

/// The wire dialect the API server speaks. Only OpenAI Chat Completions is
/// implemented today; the other two are reserved so a config written for a
/// future build still parses — requests under them answer 501 until their
/// inbound conversions land (the outbound halves already exist in
/// `provider::anthropic_compat` / `provider::gemini_compat`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerApi {
    /// OpenAI Chat Completions (`POST /v1/chat/completions`, `GET /v1/models`).
    #[default]
    Openai,
    /// Anthropic Messages (`POST /v1/messages`) — planned.
    Anthropic,
    /// Google Generative Language (`POST /v1beta/models/{m}:generateContent`) — planned.
    Gemini,
}

impl ServerApi {
    pub fn label(self) -> &'static str {
        match self {
            ServerApi::Openai => "openai",
            ServerApi::Anthropic => "anthropic",
            ServerApi::Gemini => "gemini",
        }
    }

    /// Parse a user-typed dialect name (`/serve api <kind>`).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "openai" | "oai" => Some(ServerApi::Openai),
            "anthropic" | "claude" => Some(ServerApi::Anthropic),
            "gemini" | "google" => Some(ServerApi::Gemini),
            _ => None,
        }
    }

    pub fn is_implemented(self) -> bool {
        matches!(self, ServerApi::Openai)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_kind")]
    pub kind: ProviderKind,
    #[serde(default = "default_provider_token")]
    pub token: String,
    #[serde(default = "default_provider_model")]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_provider_temperature")]
    pub temperature: f32,
    #[serde(default = "default_provider_max_tokens")]
    pub max_tokens: u32,
}

fn default_provider_kind() -> ProviderKind {
    ProviderKind::Deepseek
}

fn default_provider_token() -> String {
    String::new()
}

fn default_provider_model() -> String {
    "deepseek-chat".to_string()
}

fn default_provider_temperature() -> f32 {
    0.7
}

fn default_provider_max_tokens() -> u32 {
    4096
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: default_provider_kind(),
            token: default_provider_token(),
            model: default_provider_model(),
            base_url: None,
            temperature: default_provider_temperature(),
            max_tokens: default_provider_max_tokens(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Deepseek,
    Openai,
    Custom,
}

/// Which wire protocol a `/providers` entry speaks.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderProtocol {
    /// OpenAI Chat Completions (`POST {base}/chat/completions`) — LM
    /// Studio, Ollama's `/v1`, vLLM, OpenRouter, … The default, so entries
    /// written before this field existed keep working.
    #[default]
    Openai,
    /// Anthropic Messages (`POST {base}/messages`, `x-api-key` +
    /// `anthropic-version` headers) — the Anthropic API and
    /// Claude-compatible endpoints/proxies.
    Anthropic,
    /// Google Generative Language API (`POST
    /// {base}/models/{model}:generateContent`, `x-goog-api-key` header) —
    /// Gemini. Note Google also exposes an OpenAI-compatible layer under
    /// `{base}/openai`, but the native protocol is the first-class one.
    Gemini,
}

impl ProviderProtocol {
    pub fn label(self) -> &'static str {
        match self {
            ProviderProtocol::Openai => "openai-compat",
            ProviderProtocol::Anthropic => "anthropic-compat",
            ProviderProtocol::Gemini => "gemini",
        }
    }
}

/// One `/providers`-managed endpoint (OpenAI- or Anthropic-compatible).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderEntry {
    /// Unique display name; also the `active_provider` key. The name
    /// `"deepseek"` is reserved for the built-in provider.
    pub name: String,
    /// API base, usually ending in `/v1` (e.g. `http://localhost:11434/v1`
    /// for Ollama, `http://localhost:1234/v1` for LM Studio,
    /// `https://api.anthropic.com/v1` for Anthropic).
    pub base_url: String,
    /// Bearer token / `x-api-key`, if the endpoint needs one (local
    /// servers usually don't).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Default model id: what TUI chat sends and what the bare `<name>`
    /// API alias resolves to. May be empty (wizard allows skipping it) —
    /// the fetched model list (`[provider_models]`, `model_cache`) then
    /// drives the API catalog, and the TUI prompts for a `/models` pick on
    /// activation.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub protocol: ProviderProtocol,
}

/// The reserved name of the built-in DeepSeek web provider in `/providers`
/// listings and `active_provider`.
pub const BUILTIN_PROVIDER_NAME: &str = "deepseek";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Active theme: a preset name from `crate::tui::theme::PRESETS` or the
    /// name of a `custom_themes` entry. Unknown names fall back to the
    /// default theme — see `Theme::resolve`.
    #[serde(default = "default_ui_theme")]
    pub theme: String,
    /// User-made themes from the `/themes` wizard.
    #[serde(default)]
    pub custom_themes: Vec<CustomTheme>,
}

fn default_ui_theme() -> String {
    "default".to_string()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_ui_theme(),
            custom_themes: Vec::new(),
        }
    }
}

/// One `/themes`-wizard theme: a preset to inherit from plus per-role
/// `"#rrggbb"` overrides keyed by `crate::tui::theme::ROLES` keys. Kept as
/// a string map (not one field per role) so a palette written by a newer
/// build still loads, minus the roles this build doesn't know.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CustomTheme {
    pub name: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub colors: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_agent_max_steps_per_turn")]
    pub max_steps_per_turn: usize,
    #[serde(default = "default_agent_max_tools_per_step")]
    pub max_tools_per_step: usize,
    #[serde(default = "default_agent_rate_limit_ms")]
    pub rate_limit_ms: u64,
    /// Max requests allowed in any rolling 60s window (0 = no cap). Applied
    /// alongside `rate_limit_ms` rather than replacing it — the two catch
    /// different shapes of abuse (burst vs. steady-state).
    #[serde(default = "default_agent_rate_limit_per_minute")]
    pub rate_limit_per_minute: u32,
    #[serde(default = "default_agent_max_retries")]
    pub max_retries: i32,
}

fn default_agent_max_steps_per_turn() -> usize {
    256
}

fn default_agent_max_tools_per_step() -> usize {
    10
}

fn default_agent_rate_limit_ms() -> u64 {
    0
}

fn default_agent_rate_limit_per_minute() -> u32 {
    0
}

fn default_agent_max_retries() -> i32 {
    0
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps_per_turn: default_agent_max_steps_per_turn(),
            max_tools_per_step: default_agent_max_tools_per_step(),
            rate_limit_ms: default_agent_rate_limit_ms(),
            rate_limit_per_minute: default_agent_rate_limit_per_minute(),
            max_retries: default_agent_max_retries(),
        }
    }
}

impl AgentConfig {
    /// Short human-readable summary of both rate-limit gates, shared by the
    /// `/rate` command's confirmation message and the stats panel so they
    /// never drift out of sync (e.g. "500ms, 10/min" or "off").
    pub fn rate_limit_display(&self) -> String {
        let ms = (self.rate_limit_ms > 0).then(|| format!("{}ms", self.rate_limit_ms));
        let per_min =
            (self.rate_limit_per_minute > 0).then(|| format!("{}/min", self.rate_limit_per_minute));
        match (ms, per_min) {
            (None, None) => "off".to_string(),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => format!("{a}, {b}"),
        }
    }
}

/// `[context]` — the compaction ladder. See `.docs/context-compaction.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Master switch for the whole ladder.
    #[serde(default = "default_context_auto_compact")]
    pub auto_compact: bool,
    /// Model context window in tokens. `0` = ask the provider, falling back
    /// to no compaction if it can't say.
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    /// Headroom subtracted from the window for the model's own answer.
    #[serde(default = "default_context_reserved_tokens")]
    pub reserved_tokens: u32,
    /// Verbatim tail kept past the summary boundary, in tokens. `0` =
    /// compute as `clamp(usable * 0.25, 2000..15000)`.
    #[serde(default = "default_context_preserve_recent_tokens")]
    pub preserve_recent_tokens: u32,
    /// Characters kept per tool result before it is truncated.
    #[serde(default = "default_context_tool_output_limit")]
    pub tool_output_limit: u32,
    /// Which summary mode `/compact` uses when a chat has not overridden it.
    /// 1 = first and last turn verbatim, middle summarised; 2 = map-reduce
    /// over the oldest half; 3 = everything before the last turn.
    #[serde(default = "default_context_compact_mode")]
    pub compact_mode: u8,
}

fn default_context_auto_compact() -> bool {
    true
}

fn default_context_window() -> u32 {
    0
}

fn default_context_reserved_tokens() -> u32 {
    20_000
}

fn default_context_preserve_recent_tokens() -> u32 {
    0
}

// Cline/opencode use 2000 chars when serialising for a *summariser*; this cap
// applies at *capture* instead, where Codex uses 10 000 — 2000 here would
// cut an ordinary build log before the model ever saw it.
fn default_context_tool_output_limit() -> u32 {
    10_000
}

fn default_context_compact_mode() -> u8 {
    ContextConfig::DEFAULT_COMPACT_MODE
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            auto_compact: default_context_auto_compact(),
            context_window: default_context_window(),
            reserved_tokens: default_context_reserved_tokens(),
            preserve_recent_tokens: default_context_preserve_recent_tokens(),
            tool_output_limit: default_context_tool_output_limit(),
            compact_mode: default_context_compact_mode(),
        }
    }
}

impl ContextConfig {
    /// The mode `/compact` falls back to.
    pub const DEFAULT_COMPACT_MODE: u8 = 1;
    /// Highest mode the ladder knows.
    pub const MAX_COMPACT_MODE: u8 = 3;

    /// `compact_mode`, or `DEFAULT_COMPACT_MODE` if it is outside 1-3.
    /// Parsing stays permissive on purpose: a config written by a newer
    /// build (or hand-edited) must still load, so the value is checked here
    /// at use time instead of being rejected at parse time.
    pub fn effective_compact_mode(&self) -> u8 {
        match self.compact_mode {
            m if (1..=Self::MAX_COMPACT_MODE).contains(&m) => m,
            _ => Self::DEFAULT_COMPACT_MODE,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_mcp_cache_ttl")]
    pub cache_ttl: u64,
}

fn default_mcp_cache_ttl() -> u64 {
    300
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            cache_ttl: default_mcp_cache_ttl(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    /// How enabled skills reach the system prompt — see [`SkillInjectionMode`].
    #[serde(default)]
    pub injection: SkillInjectionMode,
}

/// Whether enabled skills are injected into the system prompt as full
/// content or as a compact `slug — description` list the model loads on
/// demand via the `skill` tool (the per-turn semantic hint suggests the
/// relevant one automatically). Full content is fine for a couple of small
/// skills and pure prompt bloat beyond that.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SkillInjectionMode {
    /// Full content while the combined size stays under
    /// [`Self::AUTO_FULL_BUDGET_BYTES`]; compact list beyond it.
    #[default]
    Auto,
    /// Always inline full content (pre-summary behavior).
    Full,
    /// Always inject the compact list.
    Summary,
}

impl SkillInjectionMode {
    /// `Auto` switches to the compact list once the enabled skills' combined
    /// content outgrows this many bytes.
    pub const AUTO_FULL_BUDGET_BYTES: usize = 8 * 1024;

    /// Resolve the mode against the combined content size of enabled skills.
    pub fn summary_for(self, total_content_bytes: usize) -> bool {
        match self {
            SkillInjectionMode::Full => false,
            SkillInjectionMode::Summary => true,
            SkillInjectionMode::Auto => total_content_bytes > Self::AUTO_FULL_BUDGET_BYTES,
        }
    }
}

/// Semantic matching (`[semantic]`) over skills and MCP tools. Enabled by
/// default: the first launch downloads the embedding model (~120 MB) into
/// `Config::data_dir()/models` once; everything after that is offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticConfig {
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    /// Max suggestions per corpus (skills, MCP tools) per turn.
    #[serde(default = "default_semantic_top_k")]
    pub top_k: usize,
    /// Dense-cosine floor a candidate must clear when it has no lexical
    /// overlap with the prompt. e5 cosines cluster high, so this is tighter
    /// than it looks — tune against the `semantic::eval` harness
    /// (`cargo test semantic::eval -- --ignored --nocapture`), not by feel.
    #[serde(default = "default_semantic_min_dense_score")]
    pub min_dense_score: f32,
    /// How MCP tool schemas reach the system prompt — see [`McpSchemaMode`].
    #[serde(default)]
    pub mcp_schemas: McpSchemaMode,
    /// Embedder batch cap — the ONNX memory guard. See [`RagLimit`].
    #[serde(default)]
    pub rag_limit: RagLimit,
}

fn default_semantic_enabled() -> bool {
    true
}

fn default_semantic_top_k() -> usize {
    3
}

fn default_semantic_min_dense_score() -> f32 {
    0.80
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            top_k: default_semantic_top_k(),
            min_dense_score: default_semantic_min_dense_score(),
            mcp_schemas: McpSchemaMode::default(),
            rag_limit: RagLimit::default(),
        }
    }
}

/// How large a batch the embedder feeds ONNX at once. This is the memory
/// guard: peak inference memory for one transformer layer is
/// `batch × heads(12) × seq(512)² × 4 B ≈ batch × 12.6 MB`, so fastembed's
/// default batch of 256 can demand multiple gigabytes and OOM on thin
/// machines (embedded/IoT hosts, small VMs). Serializes as `"auto"`,
/// `"off"`, or a bare integer in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RagLimit {
    /// Size the batch from total physical RAM (see [`RagLimit::resolve`]).
    #[default]
    Auto,
    /// No cap — fastembed's own default. For machines with RAM to spare.
    Off,
    /// A fixed batch size, exactly as given (floored to 1).
    Fixed(usize),
}

impl RagLimit {
    /// Fraction of total RAM the embed transient is allowed to occupy.
    const RAM_FRACTION: f64 = 0.05;
    /// Conservative ONNX peak per batch item at seq=512 (over the ~12.6 MB
    /// attention tensor to cover concurrent arena buffers + rounding).
    const PER_ITEM_BYTES: f64 = 48.0 * 1024.0 * 1024.0;
    /// Batch is clamped into this range regardless of RAM — 1 is the floor
    /// (0 is invalid to fastembed), 64 the ceiling (throughput plateaus and
    /// there is no reason to risk a huge allocation past it).
    const MIN_BATCH: usize = 1;
    const MAX_BATCH: usize = 64;
    /// Used when RAM can't be detected: safe on any machine with ≥ ~3 GB.
    const FALLBACK_BATCH: usize = 8;

    /// Resolve to the concrete batch argument for fastembed. `None` means
    /// "no cap" (pass fastembed its default). `total_ram_bytes` is the host's
    /// physical RAM (`None` when it couldn't be probed).
    pub fn resolve(self, total_ram_bytes: Option<u64>) -> Option<usize> {
        match self {
            RagLimit::Off => None,
            RagLimit::Fixed(n) => Some(n.max(Self::MIN_BATCH)),
            RagLimit::Auto => Some(Self::auto_batch(total_ram_bytes)),
        }
    }

    /// The auto-sized batch for a given amount of RAM. Split out for testing.
    pub fn auto_batch(total_ram_bytes: Option<u64>) -> usize {
        let Some(ram) = total_ram_bytes else {
            return Self::FALLBACK_BATCH;
        };
        let budget = ram as f64 * Self::RAM_FRACTION;
        let batch = (budget / Self::PER_ITEM_BYTES) as usize;
        batch.clamp(Self::MIN_BATCH, Self::MAX_BATCH)
    }
}

impl std::fmt::Display for RagLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RagLimit::Auto => f.write_str("auto"),
            RagLimit::Off => f.write_str("off"),
            RagLimit::Fixed(n) => write!(f, "{n}"),
        }
    }
}

impl Serialize for RagLimit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RagLimit::Auto => serializer.serialize_str("auto"),
            RagLimit::Off => serializer.serialize_str("off"),
            RagLimit::Fixed(n) => serializer.serialize_u64(*n as u64),
        }
    }
}

impl<'de> Deserialize<'de> for RagLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RagLimitVisitor;
        impl<'de> serde::de::Visitor<'de> for RagLimitVisitor {
            type Value = RagLimit;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#""auto", "off", or a positive integer"#)
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<RagLimit, E> {
                match v.trim().to_ascii_lowercase().as_str() {
                    "auto" => Ok(RagLimit::Auto),
                    "off" => Ok(RagLimit::Off),
                    other => Err(E::custom(format!(
                        "expected \"auto\", \"off\", or an integer, got {other:?}"
                    ))),
                }
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<RagLimit, E> {
                Ok(RagLimit::Fixed(v.max(1) as usize))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<RagLimit, E> {
                if v < 1 {
                    return Err(E::custom("rag_limit integer must be ≥ 1"));
                }
                Ok(RagLimit::Fixed(v as usize))
            }
        }
        deserializer.deserialize_any(RagLimitVisitor)
    }
}

/// Whether the system prompt carries full MCP tool definitions or a
/// compact name+description list with schemas fetched on demand (semantic
/// per-turn hints + the `tool_search` builtin). Deferring saves thousands
/// of prompt tokens once a few servers are connected.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpSchemaMode {
    /// Defer once the catalog outgrows `AUTO_DEFER_THRESHOLD` tools.
    #[default]
    Auto,
    /// Always inline full definitions (pre-0.2 behavior).
    Full,
    /// Always defer (any nonzero tool count).
    Deferred,
}

impl McpSchemaMode {
    /// Above this many MCP tools, `Auto` switches to deferred schemas. One
    /// connected Playwright server alone is ~25 tools, so auto-defer is the
    /// common case for anyone actually using MCP.
    pub const AUTO_DEFER_THRESHOLD: usize = 12;

    /// Resolve the mode against a live tool count. Callers that have
    /// semantic matching disabled must pass `Full` instead of consulting
    /// this — without embeddings, `tool_search` degrades to substring
    /// matching, which is too weak to be the only path to a schema.
    pub fn deferred_for(self, tool_count: usize) -> bool {
        match self {
            McpSchemaMode::Full => false,
            McpSchemaMode::Deferred => tool_count > 0,
            McpSchemaMode::Auto => tool_count > Self::AUTO_DEFER_THRESHOLD,
        }
    }
}

impl Config {
    /// The `/providers` entry currently selected, or `None` when the
    /// built-in DeepSeek provider is active (default, explicit
    /// `"deepseek"`, or a stale name that no longer exists).
    pub fn active_provider_entry(&self) -> Option<&ProviderEntry> {
        let name = self.active_provider.as_deref()?;
        if name == BUILTIN_PROVIDER_NAME {
            return None;
        }
        self.providers.iter().find(|entry| entry.name == name)
    }

    /// The model the active provider will send — the entry's model for a
    /// custom provider, `[provider].model` for the built-in one.
    pub fn active_model(&self) -> &str {
        self.active_provider_entry()
            .map(|entry| entry.model.as_str())
            .unwrap_or(&self.provider.model)
    }

    /// Display name of the active provider: the `/providers` entry's name,
    /// or the built-in provider's kind label. Shared by the status bar,
    /// landing screen, and stats panel.
    pub fn active_provider_name(&self) -> String {
        if let Some(entry) = self.active_provider_entry() {
            return entry.name.clone();
        }
        match self.provider.kind {
            ProviderKind::Deepseek => "DeepSeek",
            ProviderKind::Openai => "OpenAI",
            ProviderKind::Custom => "Custom",
        }
        .to_string()
    }

    /// MCP schema mode as the system prompt should see it. Without
    /// semantic matching there is no `tool_search` worth relying on, so
    /// deferring schemas would leave the model with no good path to them —
    /// force full inlining in that case. Shared by the TUI turn paths and
    /// the headless harness so the two build identical prompts.
    pub fn effective_mcp_schema_mode(&self) -> McpSchemaMode {
        if self.semantic.enabled {
            self.semantic.mcp_schemas
        } else {
            McpSchemaMode::Full
        }
    }

    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pooprusteek")
            .join("config.toml")
    }

    pub fn data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pooprusteek")
    }

    pub fn sessions_dir() -> PathBuf {
        Self::data_dir().join("sessions")
    }
}

/// `--config <file>` when given, the user's real config otherwise.
pub fn load_from_optional(explicit: Option<&std::path::Path>) -> AppResult<Config> {
    match explicit {
        Some(path) => load_from(path),
        None => load(),
    }
}

/// Load from an explicit file instead of the user's real config. Used by
/// `--config`, so a harness run can point at a throwaway token and data set
/// without touching the config the TUI uses.
pub fn load_from(path: &std::path::Path) -> AppResult<Config> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| AppError::Config(format!("{}: {e}", path.display())))?;
    toml::from_str(&content).map_err(|e| AppError::Config(format!("{}: {e}", path.display())))
}

pub fn load() -> AppResult<Config> {
    let path = Config::path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| AppError::Config(e.to_string()))?;
    toml::from_str(&content).map_err(|e| AppError::Config(e.to_string()))
}

pub fn save(config: &Config) -> AppResult<()> {
    let path = Config::path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AppError::Config(e.to_string()))?;
    }
    let content = toml::to_string_pretty(config).map_err(|e| AppError::Config(e.to_string()))?;
    crate::util::atomic_write(&path, content.as_bytes())
        .map_err(|e| AppError::Config(e.to_string()))?;

    // The config file holds the DeepSeek token in plaintext — restrict it to
    // the owner. No-op on Windows, which has no POSIX mode bits; ACLs are a
    // separate concern out of scope for this fix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms).map_err(|e| AppError::Config(e.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_with(rate_limit_ms: u64, rate_limit_per_minute: u32) -> AgentConfig {
        let mut agent = Config::default().agent;
        agent.rate_limit_ms = rate_limit_ms;
        agent.rate_limit_per_minute = rate_limit_per_minute;
        agent
    }

    #[test]
    fn rate_limit_display_both_zero_is_off() {
        assert_eq!(agent_with(0, 0).rate_limit_display(), "off");
    }

    #[test]
    fn rate_limit_display_ms_only() {
        assert_eq!(agent_with(500, 0).rate_limit_display(), "500ms");
    }

    #[test]
    fn rate_limit_display_per_minute_only() {
        assert_eq!(agent_with(0, 10).rate_limit_display(), "10/min");
    }

    #[test]
    fn rate_limit_display_both_set() {
        assert_eq!(agent_with(500, 10).rate_limit_display(), "500ms, 10/min");
    }

    #[test]
    fn mcp_schema_mode_resolution() {
        use McpSchemaMode::*;
        assert!(!Full.deferred_for(1000));
        assert!(!Deferred.deferred_for(0), "nothing to defer with no tools");
        assert!(Deferred.deferred_for(1));
        assert!(!Auto.deferred_for(McpSchemaMode::AUTO_DEFER_THRESHOLD));
        assert!(Auto.deferred_for(McpSchemaMode::AUTO_DEFER_THRESHOLD + 1));
    }

    #[test]
    fn semantic_config_without_mcp_schemas_field_still_loads() {
        // Stage-1 config files carry [semantic] without `mcp_schemas` —
        // the field must default instead of failing the parse.
        let toml_text = "enabled = true\ntop_k = 3\nmin_dense_score = 0.8\n";
        let parsed: SemanticConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(parsed.mcp_schemas, McpSchemaMode::Auto);
        // Same for the newer `rag_limit` field.
        assert_eq!(parsed.rag_limit, RagLimit::Auto);
    }

    #[test]
    fn semantic_config_enabled_only_still_loads() {
        // Reproduces a real harness failure: `[semantic]\nenabled = false`
        // alone died with "missing field top_k" before enabled/top_k/
        // min_dense_score got named defaults.
        let parsed: SemanticConfig = toml::from_str("enabled = false\n").unwrap();
        assert!(!parsed.enabled);
        assert_eq!(parsed.top_k, 3);
        assert_eq!(parsed.min_dense_score, 0.8);
    }

    #[test]
    fn rag_limit_auto_scales_with_ram_and_clamps() {
        // Unknown RAM → the conservative fallback.
        assert_eq!(RagLimit::auto_batch(None), RagLimit::FALLBACK_BATCH);
        // Bigger machines get bigger batches, monotonically…
        let gb = |n: u64| Some(n * 1024 * 1024 * 1024);
        let small = RagLimit::auto_batch(gb(4));
        let big = RagLimit::auto_batch(gb(32));
        assert!(small >= RagLimit::MIN_BATCH && small < big);
        // …but never past the ceiling, even on a huge host.
        assert_eq!(RagLimit::auto_batch(gb(512)), RagLimit::MAX_BATCH);
        // A tiny machine still clears the floor (0 is invalid to fastembed).
        assert!(RagLimit::auto_batch(Some(256 * 1024 * 1024)) >= RagLimit::MIN_BATCH);
    }

    #[test]
    fn rag_limit_resolve_covers_all_modes() {
        assert_eq!(RagLimit::Off.resolve(None), None);
        assert_eq!(RagLimit::Fixed(20).resolve(None), Some(20));
        // Fixed(0) is floored to a valid batch.
        assert_eq!(RagLimit::Fixed(0).resolve(None), Some(1));
        assert!(RagLimit::Auto.resolve(None).is_some());
    }

    #[test]
    fn rag_limit_round_trips_through_toml() {
        for mode in [RagLimit::Auto, RagLimit::Off, RagLimit::Fixed(12)] {
            let mut cfg = SemanticConfig::default();
            cfg.rag_limit = mode;
            let text = toml::to_string_pretty(&cfg).unwrap();
            let parsed: SemanticConfig = toml::from_str(&text).unwrap();
            assert_eq!(parsed.rag_limit, mode, "round-trip failed for {mode}");
        }
        // Bare integer and keyword strings both parse.
        let n: SemanticConfig =
            toml::from_str("enabled = true\ntop_k = 3\nmin_dense_score = 0.8\nrag_limit = 8\n")
                .unwrap();
        assert_eq!(n.rag_limit, RagLimit::Fixed(8));
        let off: SemanticConfig = toml::from_str(
            "enabled = true\ntop_k = 3\nmin_dense_score = 0.8\nrag_limit = \"off\"\n",
        )
        .unwrap();
        assert_eq!(off.rag_limit, RagLimit::Off);
    }

    #[test]
    fn compact_mode_defaults_to_one() {
        assert_eq!(ContextConfig::default().compact_mode, 1);
        // A [context] section written before the field existed still loads.
        let parsed: ContextConfig = toml::from_str("auto_compact = true\n").unwrap();
        assert_eq!(parsed.compact_mode, 1);
        assert_eq!(parsed.effective_compact_mode(), 1);
    }

    #[test]
    fn out_of_range_compact_mode_parses_and_falls_back() {
        // Parsing stays permissive; the helper is what guards the range.
        for (text, raw) in [("compact_mode = 0\n", 0), ("compact_mode = 9\n", 9)] {
            let parsed: ContextConfig = toml::from_str(text).unwrap();
            assert_eq!(parsed.compact_mode, raw);
            assert_eq!(parsed.effective_compact_mode(), 1);
        }
        let ok: ContextConfig = toml::from_str("compact_mode = 3\n").unwrap();
        assert_eq!(ok.effective_compact_mode(), 3);
    }

    #[test]
    fn ui_config_without_custom_themes_still_loads() {
        // Pre-/themes config files carry [ui] without `custom_themes` —
        // the field must default instead of failing the parse.
        let toml_text = "theme = \"default\"\nshow_status_bar = true\nshow_line_numbers = false\nmax_message_length = 4096\n";
        let parsed: UiConfig = toml::from_str(toml_text).unwrap();
        assert!(parsed.custom_themes.is_empty());
    }

    #[test]
    fn custom_theme_round_trips_through_toml() {
        let mut config = Config::default();
        config.ui.theme = "mine".to_string();
        config.ui.custom_themes.push(CustomTheme {
            name: "mine".to_string(),
            base: "nord".to_string(),
            colors: std::collections::BTreeMap::from([(
                "accent".to_string(),
                "#ff8000".to_string(),
            )]),
        });
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.custom_themes, config.ui.custom_themes);
        assert_eq!(parsed.ui.theme, "mine");
    }

    #[test]
    fn skills_config_partial_fill_still_loads() {
        // `enabled`/`paths` had no serde default at all — a section
        // carrying only `injection` used to fail the parse.
        let parsed: SkillsConfig = toml::from_str("injection = \"full\"\n").unwrap();
        assert!(parsed.enabled.is_empty());
        assert!(parsed.paths.is_empty());
        assert_eq!(parsed.injection, SkillInjectionMode::Full);
    }

    #[test]
    fn config_without_provider_entries_still_loads() {
        // A pre-/providers config.toml has no `providers` array and no
        // `active_provider` — both must default instead of failing the parse.
        let old = toml::to_string_pretty(&{
            let mut config = Config::default();
            config.provider.token = "t".to_string();
            config
        })
        .unwrap()
        .lines()
        .filter(|line| !line.contains("active_provider") && !line.contains("[[providers]]"))
        .collect::<Vec<_>>()
        .join("\n");

        let parsed: Config = toml::from_str(&old).unwrap();
        assert!(parsed.providers.is_empty());
        assert!(parsed.active_provider.is_none());
        assert!(parsed.active_provider_entry().is_none());
        assert_eq!(parsed.active_model(), "deepseek-chat");
    }

    #[test]
    fn config_without_update_section_still_loads() {
        // Pre-updater config files carry no [update] table — it must
        // default (auto off) instead of failing the parse.
        let old = toml::to_string_pretty(&Config::default())
            .unwrap()
            .lines()
            .take_while(|line| !line.contains("[update]"))
            .collect::<Vec<_>>()
            .join("\n");

        let parsed: Config = toml::from_str(&old).unwrap();
        assert!(!parsed.update.auto);

        // And the flag round-trips once set.
        let mut config = Config::default();
        config.update.auto = true;
        let text = toml::to_string_pretty(&config).unwrap();
        let reparsed: Config = toml::from_str(&text).unwrap();
        assert!(reparsed.update.auto);
    }

    #[test]
    fn update_config_empty_table_still_loads() {
        let parsed: UpdateConfig = toml::from_str("").unwrap();
        assert!(!parsed.auto);
    }

    #[test]
    fn config_without_server_section_still_loads() {
        // Pre-server config files carry no [server] table — every field
        // must default instead of failing the parse.
        let old = toml::to_string_pretty(&Config::default())
            .unwrap()
            .lines()
            .take_while(|line| !line.contains("[server]"))
            .collect::<Vec<_>>()
            .join("\n");

        let parsed: Config = toml::from_str(&old).unwrap();
        assert_eq!(parsed.server.port, ServerConfig::DEFAULT_PORT);
        assert_eq!(parsed.server.host, "127.0.0.1");
        assert_eq!(parsed.server.api, ServerApi::Openai);
        assert!(parsed.server.api_key.is_none());
    }

    #[test]
    fn server_config_partial_fill_still_loads() {
        // A section carrying only `port` must still parse.
        let parsed: ServerConfig = toml::from_str("port = 9000\n").unwrap();
        assert_eq!(parsed.port, 9000);
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.api, ServerApi::Openai);
        assert!(parsed.api_key.is_none());
    }

    #[test]
    fn provider_models_config_partial_fill_still_loads() {
        // A section carrying only `refetch_ms` must still parse.
        let parsed: ProviderModelsConfig = toml::from_str("refetch_ms = 5000\n").unwrap();
        assert_eq!(parsed.refetch_ms, 5000);
        assert_eq!(parsed.cache_ms, ProviderModelsConfig::DEFAULT_MS);
    }

    #[test]
    fn server_api_parse_accepts_aliases_and_rejects_junk() {
        assert_eq!(ServerApi::parse("OpenAI"), Some(ServerApi::Openai));
        assert_eq!(ServerApi::parse("claude"), Some(ServerApi::Anthropic));
        assert_eq!(ServerApi::parse("google"), Some(ServerApi::Gemini));
        assert_eq!(ServerApi::parse("grpc"), None);
        assert!(ServerApi::Openai.is_implemented());
        assert!(!ServerApi::Anthropic.is_implemented());
    }

    #[test]
    fn active_model_prefers_active_entry() {
        let mut config = Config::default();
        config.providers.push(ProviderEntry {
            name: "lmstudio".to_string(),
            base_url: "http://localhost:1234/v1".to_string(),
            api_key: None,
            model: "qwen".to_string(),
            protocol: ProviderProtocol::default(),
        });
        assert_eq!(config.active_model(), "deepseek-chat");
        config.active_provider = Some("lmstudio".to_string());
        assert_eq!(config.active_model(), "qwen");
        // The reserved built-in name behaves like None.
        config.active_provider = Some(BUILTIN_PROVIDER_NAME.to_string());
        assert_eq!(config.active_model(), "deepseek-chat");
    }

    #[test]
    fn empty_toml_parses_to_defaults() {
        // No sections at all — every field of every section must default.
        let parsed: Config = toml::from_str("").unwrap();
        let default = Config::default();
        assert_eq!(parsed.provider.kind, default.provider.kind);
        assert_eq!(parsed.provider.token, default.provider.token);
        assert_eq!(parsed.provider.model, default.provider.model);
        assert_eq!(parsed.provider.base_url, default.provider.base_url);
        assert_eq!(parsed.provider.temperature, default.provider.temperature);
        assert_eq!(parsed.provider.max_tokens, default.provider.max_tokens);
        assert_eq!(parsed.ui.theme, default.ui.theme);
        assert_eq!(parsed.ui.custom_themes, default.ui.custom_themes);
        assert_eq!(
            parsed.agent.max_steps_per_turn,
            default.agent.max_steps_per_turn
        );
        assert_eq!(
            parsed.agent.max_tools_per_step,
            default.agent.max_tools_per_step
        );
        assert_eq!(parsed.agent.rate_limit_ms, default.agent.rate_limit_ms);
        assert_eq!(
            parsed.agent.rate_limit_per_minute,
            default.agent.rate_limit_per_minute
        );
        assert_eq!(parsed.agent.max_retries, default.agent.max_retries);
        assert_eq!(parsed.context.auto_compact, default.context.auto_compact);
        assert_eq!(
            parsed.context.context_window,
            default.context.context_window
        );
        assert_eq!(
            parsed.context.reserved_tokens,
            default.context.reserved_tokens
        );
        assert_eq!(
            parsed.context.preserve_recent_tokens,
            default.context.preserve_recent_tokens
        );
        assert_eq!(
            parsed.context.tool_output_limit,
            default.context.tool_output_limit
        );
        assert_eq!(parsed.mcp.cache_ttl, default.mcp.cache_ttl);
    }

    #[test]
    fn provider_section_with_only_token_keeps_other_defaults() {
        // A [provider] table naming just `token` must still fill in every
        // other provider field, plus the untouched agent/ui/mcp sections.
        let parsed: Config = toml::from_str("[provider]\ntoken = \"x\"\n").unwrap();
        assert_eq!(parsed.provider.token, "x");
        assert_eq!(parsed.provider.model, "deepseek-chat");
        assert_eq!(parsed.provider.temperature, 0.7);
        assert_eq!(parsed.provider.max_tokens, 4096);
        assert_eq!(parsed.agent.max_steps_per_turn, 256);
        assert_eq!(parsed.ui.theme, "default");
        assert_eq!(parsed.mcp.cache_ttl, 300);
    }

    #[test]
    fn agent_section_with_only_max_retries_keeps_other_defaults() {
        // Regression: a bare `#[serde(default)]` would zero/false the rest
        // of [agent] instead of keeping the real defaults.
        let parsed: Config = toml::from_str("[agent]\nmax_retries = 3\n").unwrap();
        assert_eq!(parsed.agent.max_retries, 3);
        assert_eq!(parsed.agent.max_steps_per_turn, 256);
    }

    #[test]
    fn unknown_key_in_known_section_still_parses() {
        let parsed: Config =
            toml::from_str("[agent]\nmax_retries = 3\nsome_future_field = 1\n").unwrap();
        assert_eq!(parsed.agent.max_retries, 3);
    }

    #[test]
    fn context_section_with_only_tool_output_limit_keeps_other_defaults() {
        let parsed: Config = toml::from_str("[context]\ntool_output_limit = 500\n").unwrap();
        assert_eq!(parsed.context.tool_output_limit, 500);
        assert!(parsed.context.auto_compact);
        assert_eq!(parsed.context.context_window, 0);
        assert_eq!(parsed.context.reserved_tokens, 20_000);
        assert_eq!(parsed.context.preserve_recent_tokens, 0);
    }

    #[test]
    fn context_auto_compact_reads_from_context_section() {
        let parsed: Config = toml::from_str("[context]\nauto_compact = false\n").unwrap();
        assert!(!parsed.context.auto_compact);
    }

    #[test]
    fn stale_auto_compact_under_agent_section_is_ignored_not_an_error() {
        // Old configs still carry `auto_compact` under [agent] — it must be
        // silently ignored (no deny_unknown_fields), not fail to parse.
        let parsed: Config = toml::from_str("[agent]\nauto_compact = false\n").unwrap();
        assert!(parsed.context.auto_compact, "the moved-out key stays true");
    }
}
