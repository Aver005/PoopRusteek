use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    pub ui: UiConfig,
    pub agent: AgentConfig,
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
    pub kind: ProviderKind,
    pub token: String,
    pub model: String,
    pub base_url: Option<String>,
    pub temperature: f32,
    pub max_tokens: u32,
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
    pub theme: String,
    pub show_status_bar: bool,
    pub show_line_numbers: bool,
    pub max_message_length: usize,
    /// User-made themes from the `/themes` wizard.
    #[serde(default)]
    pub custom_themes: Vec<CustomTheme>,
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
    pub max_steps_per_turn: usize,
    pub max_tools_per_step: usize,
    pub max_context_messages: usize,
    pub auto_compact: bool,
    pub rate_limit_ms: u64,
    /// Max requests allowed in any rolling 60s window (0 = no cap). Applied
    /// alongside `rate_limit_ms` rather than replacing it — the two catch
    /// different shapes of abuse (burst vs. steady-state).
    #[serde(default)]
    pub rate_limit_per_minute: u32,
    pub max_retries: i32,
}

impl AgentConfig {
    /// Short human-readable summary of both rate-limit gates, shared by the
    /// `/rate` command's confirmation message and the stats panel so they
    /// never drift out of sync (e.g. "500ms, 10/min" or "off").
    pub fn rate_limit_display(&self) -> String {
        let ms = (self.rate_limit_ms > 0).then(|| format!("{}ms", self.rate_limit_ms));
        let per_min = (self.rate_limit_per_minute > 0)
            .then(|| format!("{}/min", self.rate_limit_per_minute));
        match (ms, per_min) {
            (None, None) => "off".to_string(),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => format!("{a}, {b}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub cache_ttl: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { cache_ttl: 300 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub enabled: Vec<String>,
    pub paths: Vec<String>,
}

/// Semantic matching (`[semantic]`) over skills and MCP tools. Enabled by
/// default: the first launch downloads the embedding model (~120 MB) into
/// `Config::data_dir()/models` once; everything after that is offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticConfig {
    pub enabled: bool,
    /// Max suggestions per corpus (skills, MCP tools) per turn.
    pub top_k: usize,
    /// Dense-cosine floor a candidate must clear when it has no lexical
    /// overlap with the prompt. e5 cosines cluster high, so this is tighter
    /// than it looks — tune against the `semantic::eval` harness
    /// (`cargo test semantic::eval -- --ignored --nocapture`), not by feel.
    pub min_dense_score: f32,
    /// How MCP tool schemas reach the system prompt — see [`McpSchemaMode`].
    #[serde(default)]
    pub mcp_schemas: McpSchemaMode,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_k: 3,
            min_dense_score: 0.80,
            mcp_schemas: McpSchemaMode::default(),
        }
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

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig {
                kind: ProviderKind::Deepseek,
                token: String::new(),
                model: "deepseek-chat".to_string(),
                base_url: None,
                temperature: 0.7,
                max_tokens: 4096,
            },
            ui: UiConfig {
                theme: "default".to_string(),
                show_status_bar: true,
                show_line_numbers: false,
                max_message_length: 4096,
                custom_themes: Vec::new(),
            },
            agent: AgentConfig {
                max_steps_per_turn: 256,
                max_tools_per_step: 10,
                max_context_messages: 256,
                auto_compact: true,
                rate_limit_ms: 0,
                rate_limit_per_minute: 0,
                max_retries: 0,
            },
            mcp: McpConfig::default(),
            skills: SkillsConfig::default(),
            semantic: SemanticConfig::default(),
            providers: Vec::new(),
            active_provider: None,
            server: ServerConfig::default(),
            provider_models: ProviderModelsConfig::default(),
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
}
