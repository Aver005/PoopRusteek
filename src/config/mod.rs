use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    pub ui: UiConfig,
    pub agent: AgentConfig,
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
            },
        }
    }
}

impl Config {
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
    std::fs::write(path, content).map_err(|e| AppError::Config(e.to_string()))
}
