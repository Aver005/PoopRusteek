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

/// One `/providers`-managed OpenAI-compatible endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderEntry {
    /// Unique display name; also the `active_provider` key. The name
    /// `"deepseek"` is reserved for the built-in provider.
    pub name: String,
    /// API base, usually ending in `/v1` (e.g. `http://localhost:11434/v1`
    /// for Ollama, `http://localhost:1234/v1` for LM Studio).
    pub base_url: String,
    /// Bearer token, if the endpoint needs one (local servers usually don't).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Model id sent with every request.
    pub model: String,
}

/// The reserved name of the built-in DeepSeek web provider in `/providers`
/// listings and `active_provider`.
pub const BUILTIN_PROVIDER_NAME: &str = "deepseek";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,
    pub show_status_bar: bool,
    pub show_line_numbers: bool,
    pub max_message_length: usize,
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
            providers: Vec::new(),
            active_provider: None,
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
    fn active_model_prefers_active_entry() {
        let mut config = Config::default();
        config.providers.push(ProviderEntry {
            name: "lmstudio".to_string(),
            base_url: "http://localhost:1234/v1".to_string(),
            api_key: None,
            model: "qwen".to_string(),
        });
        assert_eq!(config.active_model(), "deepseek-chat");
        config.active_provider = Some("lmstudio".to_string());
        assert_eq!(config.active_model(), "qwen");
        // The reserved built-in name behaves like None.
        config.active_provider = Some(BUILTIN_PROVIDER_NAME.to_string());
        assert_eq!(config.active_model(), "deepseek-chat");
    }
}
