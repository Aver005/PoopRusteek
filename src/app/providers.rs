//! State + pure logic for the `/providers` panel and its add-wizard:
//! listing the built-in DeepSeek provider alongside `/providers`-managed
//! OpenAI-compatible entries, switching the active one, and collecting a
//! new entry via the step-by-step wizard or the quick
//! `<name> <base_url> [model] [api_key]` command form. Kept as its own
//! module (like `mcp_add.rs`) so the flow's state and validation stay
//! cohesive instead of scattered across the key handlers.

use crate::app::input::InputState;
use crate::config::{Config, ProviderEntry, BUILTIN_PROVIDER_NAME};

/// Live state of the `/providers` full-screen panel.
#[derive(Debug, Clone, Default)]
pub struct ProvidersViewState {
    pub selected: usize,
    pub status_message: String,
}

/// One row of the panel: the built-in DeepSeek provider first, then every
/// config entry in order.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub name: String,
    pub detail: String,
    pub active: bool,
    pub builtin: bool,
}

pub fn provider_rows(config: &Config) -> Vec<ProviderRow> {
    let custom_active = config.active_provider_entry().map(|entry| entry.name.clone());
    let mut rows = vec![ProviderRow {
        name: BUILTIN_PROVIDER_NAME.to_string(),
        detail: format!(
            "built-in DeepSeek web · {} · {}",
            config.provider.model,
            if config.provider.token.is_empty() { "no token" } else { "token set" },
        ),
        active: custom_active.is_none(),
        builtin: true,
    }];
    for entry in &config.providers {
        rows.push(ProviderRow {
            name: entry.name.clone(),
            detail: format!(
                "{} · {}{}",
                entry.base_url,
                entry.model,
                if entry.api_key.is_some() { " · key set" } else { "" },
            ),
            active: custom_active.as_deref() == Some(entry.name.as_str()),
            builtin: false,
        });
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderWizardStep {
    Name,
    BaseUrl,
    ApiKey,
    Model,
    Confirm,
}

impl ProviderWizardStep {
    pub fn number(self) -> usize {
        match self {
            ProviderWizardStep::Name => 1,
            ProviderWizardStep::BaseUrl => 2,
            ProviderWizardStep::ApiKey => 3,
            ProviderWizardStep::Model => 4,
            ProviderWizardStep::Confirm => 5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ProviderWizardStep::Name => "Name",
            ProviderWizardStep::BaseUrl => "Base URL (usually ends with /v1)",
            ProviderWizardStep::ApiKey => "API key (blank = none)",
            ProviderWizardStep::Model => "Model id",
            ProviderWizardStep::Confirm => "Confirm",
        }
    }
}

/// The `/providers add` wizard: four text steps + confirm. All fields are
/// single-line (`apply_text_key` with newlines off).
#[derive(Debug, Clone)]
pub struct ProviderAddState {
    pub step: ProviderWizardStep,
    pub name: InputState,
    pub base_url: InputState,
    pub api_key: InputState,
    pub model: InputState,
    pub error: Option<String>,
}

impl ProviderAddState {
    pub fn new() -> Self {
        Self {
            step: ProviderWizardStep::Name,
            name: InputState::default(),
            base_url: InputState::default(),
            api_key: InputState::default(),
            model: InputState::default(),
            error: None,
        }
    }

    /// Build the final entry from the collected fields. Steps validate on
    /// advance, so failures here mean the two validations disagree — kept
    /// fallible rather than assumed-infallible (same stance as
    /// `mcp_add::WizardState::build_config`).
    pub fn build_entry(&self, config: &Config) -> Result<ProviderEntry, String> {
        let name = self.name.buffer.trim().to_string();
        validate_name(&name, config)?;
        let base_url = validate_base_url(self.base_url.buffer.trim())?;
        let model = self.model.buffer.trim().to_string();
        if model.is_empty() {
            return Err("model can't be empty".to_string());
        }
        let api_key = self.api_key.buffer.trim();
        Ok(ProviderEntry {
            name,
            base_url,
            api_key: (!api_key.is_empty()).then(|| api_key.to_string()),
            model,
        })
    }
}

pub fn validate_name(name: &str, config: &Config) -> Result<(), String> {
    if name.is_empty() {
        return Err("name can't be empty".to_string());
    }
    if name == BUILTIN_PROVIDER_NAME {
        return Err(format!("'{BUILTIN_PROVIDER_NAME}' is the built-in provider"));
    }
    if config.providers.iter().any(|entry| entry.name == name) {
        return Err(format!("'{name}' already exists"));
    }
    Ok(())
}

/// Normalized (trailing-slash-trimmed) base URL, or a human-readable error.
pub fn validate_base_url(url: &str) -> Result<String, String> {
    if url.is_empty() {
        return Err("base URL can't be empty".to_string());
    }
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("unsupported URL scheme: {}", parsed.scheme()));
    }
    Ok(url.trim_end_matches('/').to_string())
}

/// Parse the quick command form: `<name> <base_url> [model] [api_key]`.
/// Model defaults to `"default"` (fine for LM Studio, which serves its
/// loaded model regardless; Ollama needs a real model id — use the wizard
/// or pass it explicitly).
pub fn parse_quick_add(raw: &str, config: &Config) -> Result<ProviderEntry, String> {
    let mut tokens = raw.split_whitespace();
    let name = tokens.next().ok_or_else(|| "expected: <name> <base_url> [model] [api_key]".to_string())?;
    let base_url = tokens.next().ok_or_else(|| "expected a base URL after the name".to_string())?;
    let model = tokens.next().unwrap_or("default");
    let api_key = tokens.next();
    if tokens.next().is_some() {
        return Err("too many arguments — expected: <name> <base_url> [model] [api_key]".to_string());
    }

    validate_name(name, config)?;
    let base_url = validate_base_url(base_url)?;
    Ok(ProviderEntry {
        name: name.to_string(),
        base_url,
        api_key: api_key.map(str::to_string),
        model: model.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(entries: Vec<ProviderEntry>, active: Option<&str>) -> Config {
        let mut config = Config::default();
        config.providers = entries;
        config.active_provider = active.map(str::to_string);
        config
    }

    fn entry(name: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            base_url: "http://localhost:1234/v1".to_string(),
            api_key: None,
            model: "default".to_string(),
        }
    }

    #[test]
    fn rows_mark_builtin_active_by_default() {
        let rows = provider_rows(&config_with(vec![entry("lmstudio")], None));
        assert_eq!(rows.len(), 2);
        assert!(rows[0].builtin && rows[0].active);
        assert!(!rows[1].builtin && !rows[1].active);
    }

    #[test]
    fn rows_mark_custom_active() {
        let rows = provider_rows(&config_with(vec![entry("lmstudio")], Some("lmstudio")));
        assert!(!rows[0].active);
        assert!(rows[1].active);
    }

    #[test]
    fn stale_active_name_falls_back_to_builtin() {
        let rows = provider_rows(&config_with(vec![entry("lmstudio")], Some("gone")));
        assert!(rows[0].active, "unknown active name must fall back to the built-in provider");
    }

    #[test]
    fn quick_add_minimal_form() {
        let parsed = parse_quick_add("ollama http://localhost:11434/v1 llama3", &Config::default()).unwrap();
        assert_eq!(parsed.name, "ollama");
        assert_eq!(parsed.base_url, "http://localhost:11434/v1");
        assert_eq!(parsed.model, "llama3");
        assert_eq!(parsed.api_key, None);
    }

    #[test]
    fn quick_add_defaults_model_and_trims_trailing_slash() {
        let parsed = parse_quick_add("lmstudio http://localhost:1234/v1/", &Config::default()).unwrap();
        assert_eq!(parsed.model, "default");
        assert_eq!(parsed.base_url, "http://localhost:1234/v1");
    }

    #[test]
    fn quick_add_rejects_reserved_duplicate_and_bad_urls() {
        let config = config_with(vec![entry("lmstudio")], None);
        assert!(parse_quick_add("deepseek http://x/v1", &config).is_err());
        assert!(parse_quick_add("lmstudio http://x/v1", &config).is_err());
        assert!(parse_quick_add("new not-a-url", &config).is_err());
        assert!(parse_quick_add("new ftp://host/v1", &config).is_err());
        assert!(parse_quick_add("toomany http://x/v1 model key extra", &config).is_err());
    }

    #[test]
    fn wizard_builds_entry_with_optional_key() {
        let mut wizard = ProviderAddState::new();
        wizard.name.buffer = "vllm".to_string();
        wizard.base_url.buffer = "https://gpu-box:8000/v1/".to_string();
        wizard.model.buffer = "qwen".to_string();
        let entry = wizard.build_entry(&Config::default()).unwrap();
        assert_eq!(entry.base_url, "https://gpu-box:8000/v1");
        assert_eq!(entry.api_key, None);

        wizard.api_key.buffer = "sk-123".to_string();
        let entry = wizard.build_entry(&Config::default()).unwrap();
        assert_eq!(entry.api_key.as_deref(), Some("sk-123"));
    }

    #[test]
    fn wizard_rejects_empty_model() {
        let mut wizard = ProviderAddState::new();
        wizard.name.buffer = "x".to_string();
        wizard.base_url.buffer = "http://h/v1".to_string();
        assert!(wizard.build_entry(&Config::default()).is_err());
    }
}
