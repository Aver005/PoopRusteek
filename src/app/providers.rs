//! Остатки старого `/providers`: быстрая форма добавления, проверка адреса, обновление списков
//! моделей и переключение модели. Экран и мастер теперь в `app::gateway_view`.

use crate::config::{BUILTIN_PROVIDER_NAME, Config, ProviderEntry, ProviderProtocol};

/// Fetch model lists for `entries` off the event loop and report back via
/// `AppEvent::ProviderModelsRefreshed`. The one spawn point for every
/// TUI-side refresh: startup (respects `cache_ms`), the periodic refetch,
/// and provider adds (both `force`).
pub fn spawn_models_refresh(
    cache: std::sync::Arc<crate::provider::model_cache::ProviderModelCache>,
    entries: Vec<ProviderEntry>,
    cache_ttl_ms: u64,
    force: bool,
    event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::events::AppEvent>,
) {
    if entries.is_empty() && !cache.gateway_enabled() {
        return;
    }
    tokio::spawn(async move {
        let outcome = cache.refresh(&entries, cache_ttl_ms, force).await;
        let _ = event_tx.send(crate::app::events::AppEvent::ProviderModelsRefreshed {
            summary: outcome.summary(),
            failed: outcome.failed.len(),
        });
    });
}

/// Example base URL shown under the wizard's Base URL step.
pub fn base_url_example(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Openai => {
            "e.g. http://localhost:1234/v1 (LM Studio), http://localhost:11434/v1 (Ollama), https://api.x.ai/v1 (Grok)"
        }
        ProviderProtocol::Anthropic => "e.g. https://api.anthropic.com/v1",
        ProviderProtocol::Gemini => "e.g. https://generativelanguage.googleapis.com/v1beta",
    }
}

pub fn validate_name(name: &str, config: &Config) -> Result<(), String> {
    if name.is_empty() {
        return Err("name can't be empty".to_string());
    }
    if name == BUILTIN_PROVIDER_NAME {
        return Err(format!(
            "'{BUILTIN_PROVIDER_NAME}' is the built-in provider"
        ));
    }
    if config.providers.iter().any(|entry| entry.name == name)
        || config
            .accounts
            .iter()
            .any(|a| a.id.as_str() == name || a.display_name().eq_ignore_ascii_case(name))
    {
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

/// Parse the quick command form:
/// `<name> [openai|anthropic] <base_url> [model] [api_key]`.
/// Protocol defaults to OpenAI-compatible. Model defaults to `"default"`
/// (fine for LM Studio, which serves its loaded model regardless; Ollama
/// and Anthropic need a real model id — use the wizard or pass it
/// explicitly).
pub fn parse_quick_add(raw: &str, config: &Config) -> Result<ProviderEntry, String> {
    const FORM: &str = "<name> [openai|anthropic|gemini] <base_url> [model] [api_key]";
    let mut tokens = raw.split_whitespace().peekable();
    let name = tokens.next().ok_or_else(|| format!("expected: {FORM}"))?;
    let protocol = match tokens.peek() {
        Some(&"openai") => {
            tokens.next();
            ProviderProtocol::Openai
        }
        Some(&"anthropic") => {
            tokens.next();
            ProviderProtocol::Anthropic
        }
        Some(&"gemini") => {
            tokens.next();
            ProviderProtocol::Gemini
        }
        _ => ProviderProtocol::Openai,
    };
    let base_url = tokens
        .next()
        .ok_or_else(|| "expected a base URL after the name".to_string())?;
    let model = tokens.next().unwrap_or("default");
    let api_key = tokens.next();
    if tokens.next().is_some() {
        return Err(format!("too many arguments — expected: {FORM}"));
    }

    validate_name(name, config)?;
    let base_url = validate_base_url(base_url)?;
    Ok(ProviderEntry {
        name: name.to_string(),
        base_url,
        api_key: api_key.map(str::to_string),
        model: model.to_string(),
        protocol,
        tools: crate::config::ToolProtocol::default(),
    })
}

impl crate::app::App {
    /// Tick-driven periodic model refetch (`[provider_models] refetch_ms`).
    /// Cheap elapsed check on the event loop; the fetch itself is spawned.
    pub(crate) fn maybe_refetch_provider_models(&mut self) {
        let period_ms = self.config.provider_models.refetch_ms;
        if period_ms == 0 || (self.config.providers.is_empty() && !self.gateway.is_enabled()) {
            return;
        }
        if (self.last_models_refetch.elapsed().as_millis() as u64) < period_ms {
            return;
        }
        self.last_models_refetch = std::time::Instant::now();
        spawn_models_refresh(
            std::sync::Arc::clone(&self.provider_models),
            self.config.providers.clone(),
            self.config.provider_models.cache_ms,
            true,
            self.event_tx.clone(),
        );
    }

    /// `/refetch-providers` + `/cache-providers` effects.
    pub(crate) fn apply_provider_models_action(
        &mut self,
        action: crate::commands::ProviderModelsAction,
    ) {
        use crate::commands::ProviderModelsAction;
        let describe = |ms: u64| {
            if ms == 0 {
                "off".to_string()
            } else {
                format!("{ms}ms")
            }
        };
        match action {
            ProviderModelsAction::Show => {
                let snapshot = self.provider_models.snapshot();
                let mut lines: Vec<String> = self
                    .config
                    .providers
                    .iter()
                    .map(|entry| {
                        match (
                            snapshot.get(&entry.name),
                            self.provider_models.age_ms(&entry.name),
                        ) {
                            (Some(models), Some(age)) => format!(
                                "\u{2022} {} — {} models (fetched {}s ago)",
                                entry.name,
                                models.len(),
                                age / 1000,
                            ),
                            _ => format!("\u{2022} {} — not fetched yet", entry.name),
                        }
                    })
                    .collect();
                if lines.is_empty() {
                    lines.push("\u{2022} (no /providers entries)".to_string());
                }
                self.state.push_system(&format!(
                    "Provider model lists — fetched from each entry's GET /models; they feed the \
                     API server catalog (/v1/models) and model routing.\n\
                     \n\
                     Refetch: every {} (/refetch-providers <ms|off>)\n\
                     Cache:   valid {} across restarts (/cache-providers <ms|off>)\n\
                     \n\
                     {}",
                    describe(self.config.provider_models.refetch_ms),
                    describe(self.config.provider_models.cache_ms),
                    lines.join("\n"),
                ));
            }
            ProviderModelsAction::SetRefetch(ms) => {
                let previous = self.config.provider_models.refetch_ms;
                self.config.provider_models.refetch_ms = ms;
                if let Err(message) = crate::config::save_or_message(&self.config) {
                    self.config.provider_models.refetch_ms = previous;
                    self.state.push_system(&message);
                    return;
                }
                // New period counts from now, not from the last fire.
                self.last_models_refetch = std::time::Instant::now();
                self.state.push_system(&match ms {
                    0 => "Provider model refetch disabled (still fetched on startup and provider add).".to_string(),
                    _ => format!("Provider models will be refetched every {ms}ms."),
                });
            }
            ProviderModelsAction::SetCache(ms) => {
                let previous = self.config.provider_models.cache_ms;
                self.config.provider_models.cache_ms = ms;
                if let Err(message) = crate::config::save_or_message(&self.config) {
                    self.config.provider_models.cache_ms = previous;
                    self.state.push_system(&message);
                    return;
                }
                self.state.push_system(&match ms {
                    0 => "Provider model cache disabled — every startup re-fetches.".to_string(),
                    _ => format!("Provider model lists stay valid for {ms}ms across restarts."),
                });
            }
        }
    }

    /// `/models` entry point: fetch the active provider's model list off
    /// the event loop and report back via `AppEvent::ModelsListed`. With
    /// `switch_to: Some(id)` the handler validates and switches instead of
    /// opening the picker.
    pub(crate) fn request_models(&mut self, switch_to: Option<String>) {
        let Some(provider) = self.state.focused().provider.clone() else {
            self.state
                .push_system("No active provider — configure one via /providers first.");
            return;
        };
        self.state.status_message = "Fetching models...".to_string();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = provider
                .list_models()
                .await
                .map_err(|error| error.to_string());
            let _ = event_tx.send(crate::app::events::AppEvent::ModelsListed { result, switch_to });
        });
    }

    /// Apply a `ModelsListed` result: validate-and-switch, or open the
    /// model picker with the current model marked.
    pub(crate) fn handle_models_listed(
        &mut self,
        result: Result<Vec<String>, String>,
        switch_to: Option<String>,
    ) {
        use crate::app::events::{Modal, PickerItem, PickerKind, PickerMode, PickerState};

        let models = match result {
            Ok(models) if models.is_empty() => {
                self.state.push_system("The provider reported no models.");
                return;
            }
            Ok(models) => models,
            Err(error) => {
                self.state
                    .push_system(&format!("Failed to list models: {error}"));
                return;
            }
        };

        if let Some(target) = switch_to {
            if models.iter().any(|model| model == &target) {
                self.apply_model_switch(&target);
            } else {
                self.state.push_system(&format!(
                    "Model '{target}' not found on the active provider (404). Available: {}",
                    models.join(", "),
                ));
            }
            return;
        }

        let current = self.config.active_model().to_string();
        let items: Vec<PickerItem> = models
            .into_iter()
            .map(|model| {
                let marker = if model == current { "\u{25CF}" } else { " " };
                PickerItem::new(format!("{marker} {model}"), model)
            })
            .collect();
        self.state.status_message.clear();
        self.state.modal = Some(Modal::Picker(PickerState::new_with_kind(
            " Models (Enter to switch)",
            items,
            PickerMode::Single,
            PickerKind::Models,
        )));
    }

    /// Со шлюзом модель меняется у аккаунта, что отвечает сейчас (а у DeepSeek — в `[provider]`).
    fn switch_account_model(&mut self, model: &str) {
        use crate::gateway::types::AccountProvider;
        let snapshot = self.gateway.snapshot();
        let current = crate::app::gateway_view::rows::current_account(&snapshot)
            .or_else(|| {
                crate::app::gateway_view::edits::main_route(&snapshot)
                    .and_then(|r| r.steps.first())
                    .map(|s| s.account.to_string())
            })
            .and_then(|id| crate::gateway::types::AccountId::parse(&id).ok());
        let Some(account) = current.and_then(|id| self.account_config(&id)) else {
            self.state
                .push_system("No account is answering yet: add one in /accounts.");
            return;
        };
        if account.provider == AccountProvider::DeepseekWeb {
            self.config.provider.model = model.to_string();
            if let Err(message) = crate::config::save_or_message(&self.config) {
                self.state.push_system(&message);
                return;
            }
            // Адаптер DeepSeek читает модель при сборке: пересобираем шлюз.
            self.restart_gateway();
            self.rebuild_provider();
        } else {
            let mut next = account.clone();
            next.model = Some(model.to_string());
            if !self.gateway_edit(
                crate::gateway::handle::ConfigEdit::UpsertAccount(Box::new(next)),
                "",
            ) {
                let reason = self
                    .state
                    .gateway_view
                    .screen
                    .notice
                    .as_ref()
                    .map_or_else(String::new, |n| n.text.clone());
                self.state.push_system(&reason);
                return;
            }
            self.invalidate_provider_context_window();
        }
        self.state
            .push_system(&format!("{} now uses {model}.", account.display_name()));
    }

    /// Switch the active provider's model: the active `/providers` entry's
    /// `model` field, or `[provider].model` for the built-in DeepSeek.
    pub(crate) fn apply_model_switch(&mut self, model: &str) {
        if self.gateway.is_enabled() {
            self.switch_account_model(model);
            return;
        }
        let active_name = self
            .config
            .active_provider_entry()
            .map(|entry| entry.name.clone());
        match active_name {
            Some(name) => {
                if let Some(entry) = self
                    .config
                    .providers
                    .iter_mut()
                    .find(|entry| entry.name == name)
                {
                    entry.model = model.to_string();
                }
            }
            None => {
                self.config.provider.model = model.to_string();
            }
        }
        match crate::config::save_or_message(&self.config) {
            Ok(()) => {
                self.rebuild_provider();
                self.state
                    .push_system(&format!("Model switched to {model}."));
            }
            Err(message) => {
                // The model changed in memory even though the save failed, so
                // the window the old one reported no longer describes it.
                self.invalidate_provider_context_window();
                self.state.push_system(&message);
            }
        }
    }
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
            protocol: ProviderProtocol::default(),
            tools: Default::default(),
        }
    }

    #[test]
    fn quick_add_minimal_form() {
        let parsed = parse_quick_add(
            "ollama http://localhost:11434/v1 llama3",
            &Config::default(),
        )
        .unwrap();
        assert_eq!(parsed.name, "ollama");
        assert_eq!(parsed.base_url, "http://localhost:11434/v1");
        assert_eq!(parsed.model, "llama3");
        assert_eq!(parsed.api_key, None);
    }

    #[test]
    fn quick_add_defaults_model_and_trims_trailing_slash() {
        let parsed =
            parse_quick_add("lmstudio http://localhost:1234/v1/", &Config::default()).unwrap();
        assert_eq!(parsed.model, "default");
        assert_eq!(parsed.base_url, "http://localhost:1234/v1");
        assert_eq!(parsed.protocol, ProviderProtocol::Openai);
    }

    #[test]
    fn quick_add_accepts_protocol_token() {
        let parsed = parse_quick_add(
            "claude anthropic https://api.anthropic.com/v1 claude-sonnet-4-5 sk-ant-x",
            &Config::default(),
        )
        .unwrap();
        assert_eq!(parsed.protocol, ProviderProtocol::Anthropic);
        assert_eq!(parsed.model, "claude-sonnet-4-5");
        assert_eq!(parsed.api_key.as_deref(), Some("sk-ant-x"));

        // Explicit "openai" token is also accepted (and not eaten as a URL).
        let parsed =
            parse_quick_add("lm openai http://localhost:1234/v1", &Config::default()).unwrap();
        assert_eq!(parsed.protocol, ProviderProtocol::Openai);
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
}

impl crate::app::App {
    /// Фоновая дозагрузка списков моделей. Тихо на успехе — это служебная
    /// работа; отказ показываем строкой, иначе мёртвый эндпоинт молчит.
    pub(crate) fn on_provider_models_refreshed(&mut self, summary: String, failed: usize) {
        self.state.status_message = summary.clone();
        if failed > 0 {
            self.state
                .focused_mut()
                .messages
                .push(crate::provider::ChatMessage::ui_system(&format!(
                    "⚠ {summary}"
                )));
        }
    }
}
