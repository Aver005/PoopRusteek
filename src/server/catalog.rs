//! Model-id → backend resolution for the API server. Pure functions over
//! the settings snapshot plus the fetched-model map — no providers, no
//! network — so the routing rules are testable in isolation and reusable
//! by the `/serve` status display.
//!
//! Exposed model ids:
//! - `deepseek-chat` / `deepseek-reasoner` — the built-in DeepSeek web
//!   client (when a token is configured); also reachable as `deepseek/<id>`.
//! - `<entry>/<model>` — a `/providers` entry with an explicit model: its
//!   configured default plus every model fetched from the upstream's
//!   `GET /models` (`provider::model_cache`). The `<model>` half is passed
//!   through even when unlisted, so anything the endpoint serves works.
//! - `<entry>` — a `/providers` entry by bare name → its configured model,
//!   or (when the default is unset) the first fetched model.
//! - a bare model id that equals some entry's configured model — or
//!   appears in some entry's fetched list — resolves to that entry, so
//!   clients that copy an id from upstream docs (`qwen2.5-coder`,
//!   `claude-sonnet-4-5`, …) work unprefixed.

use crate::config::{BUILTIN_PROVIDER_NAME, ProviderEntry};
use std::collections::HashMap;

/// `entry name → fetched model ids` — a `ProviderModelCache::snapshot()`.
pub type FetchedModels = HashMap<String, Vec<String>>;

/// The built-in DeepSeek web client's fixed model pair (the web API has no
/// model-listing endpoint; `resolve_model_type` maps these to its
/// `default`/`expert` mode).
pub const DEEPSEEK_MODELS: [&str; 2] = ["deepseek-chat", "deepseek-reasoner"];

/// Where one completion request should run.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedModel {
    /// The built-in DeepSeek web client; `model` picks chat vs reasoner.
    Deepseek { model: String },
    /// A `/providers` entry — `entry.model` already holds the effective
    /// (possibly caller-overridden) model to send upstream.
    Entry { entry: ProviderEntry },
}

impl ResolvedModel {
    pub fn is_deepseek(&self) -> bool {
        matches!(self, ResolvedModel::Deepseek { .. })
    }

    /// The model string the internal `CompletionRequest` should carry.
    pub fn internal_model(&self) -> &str {
        match self {
            ResolvedModel::Deepseek { model } => model,
            ResolvedModel::Entry { entry } => &entry.model,
        }
    }
}

/// Every model id `GET /v1/models` advertises, in stable order: the
/// DeepSeek pair first, then per entry its configured default followed by
/// the fetched list (minus the default — no duplicate row).
pub fn list_model_ids(
    has_deepseek: bool,
    entries: &[ProviderEntry],
    fetched: &FetchedModels,
) -> Vec<String> {
    let mut ids = Vec::new();
    if has_deepseek {
        ids.extend(DEEPSEEK_MODELS.iter().map(|id| id.to_string()));
    }
    for entry in entries {
        if !entry.model.is_empty() {
            ids.push(format!("{}/{}", entry.name, entry.model));
        }
        for model in fetched.get(&entry.name).into_iter().flatten() {
            if *model != entry.model {
                ids.push(format!("{}/{model}", entry.name));
            }
        }
    }
    ids
}

fn entry_with_model(entry: &ProviderEntry, model: &str) -> ResolvedModel {
    let mut entry = entry.clone();
    entry.model = model.to_string();
    ResolvedModel::Entry { entry }
}

/// Resolve a requested model id to a backend. `Err` carries a
/// caller-facing message (it becomes the 404 error envelope).
pub fn resolve_model(
    model_id: &str,
    has_deepseek: bool,
    entries: &[ProviderEntry],
    fetched: &FetchedModels,
) -> Result<ResolvedModel, String> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return Err("request has no model id".to_string());
    }

    let deepseek_named = |model: &str| {
        DEEPSEEK_MODELS
            .iter()
            .any(|known| model.eq_ignore_ascii_case(known))
    };

    if has_deepseek && deepseek_named(model_id) {
        return Ok(ResolvedModel::Deepseek {
            model: model_id.to_ascii_lowercase(),
        });
    }

    // `<prefix>/<rest>` — an explicit backend pick. The rest may itself
    // contain slashes (OpenRouter-style `org/model` ids), so split once.
    if let Some((prefix, rest)) = model_id.split_once('/')
        && !rest.trim().is_empty()
    {
        let rest = rest.trim();
        if prefix.eq_ignore_ascii_case(BUILTIN_PROVIDER_NAME) {
            if has_deepseek && deepseek_named(rest) {
                return Ok(ResolvedModel::Deepseek {
                    model: rest.to_ascii_lowercase(),
                });
            }
            return Err(format!(
                "unknown deepseek model '{rest}' (available: {})",
                DEEPSEEK_MODELS.join(", ")
            ));
        }
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(prefix))
        {
            return Ok(entry_with_model(entry, rest));
        }
    }

    // Bare entry name → its configured model, or the first fetched one
    // when no default is set.
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(model_id))
    {
        if !entry.model.is_empty() {
            return Ok(ResolvedModel::Entry {
                entry: entry.clone(),
            });
        }
        if let Some(first) = fetched.get(&entry.name).and_then(|models| models.first()) {
            return Ok(entry_with_model(entry, first));
        }
        return Err(format!(
            "provider '{}' has no default model — request '{}/<model>' explicitly",
            entry.name, entry.name
        ));
    }

    // Bare model id that some entry is configured for.
    if let Some(entry) = entries
        .iter()
        .find(|entry| !entry.model.is_empty() && entry.model.eq_ignore_ascii_case(model_id))
    {
        return Ok(ResolvedModel::Entry {
            entry: entry.clone(),
        });
    }

    // Bare model id that appears in some entry's fetched list.
    for entry in entries {
        if let Some(hit) = fetched
            .get(&entry.name)
            .into_iter()
            .flatten()
            .find(|model| model.eq_ignore_ascii_case(model_id))
        {
            return Ok(entry_with_model(entry, hit));
        }
    }

    Err(format!(
        "model '{model_id}' not found — see GET /v1/models for what this server exposes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProtocol;

    fn entry(name: &str, model: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            base_url: format!("http://localhost:1234/{name}"),
            api_key: None,
            model: model.to_string(),
            protocol: ProviderProtocol::Openai,
            tools: Default::default(),
        }
    }

    fn fetched(pairs: &[(&str, &[&str])]) -> FetchedModels {
        pairs
            .iter()
            .map(|(name, models)| {
                (
                    name.to_string(),
                    models.iter().map(|m| m.to_string()).collect(),
                )
            })
            .collect()
    }

    fn none() -> FetchedModels {
        FetchedModels::new()
    }

    #[test]
    fn lists_deepseek_pair_then_entries() {
        let ids = list_model_ids(true, &[entry("lmstudio", "qwen")], &none());
        assert_eq!(
            ids,
            vec!["deepseek-chat", "deepseek-reasoner", "lmstudio/qwen"]
        );
        // No token → the built-in pair disappears.
        assert_eq!(list_model_ids(false, &[], &none()), Vec::<String>::new());
    }

    #[test]
    fn lists_fetched_models_without_duplicating_the_default() {
        let entries = [entry("lm", "qwen")];
        let fetched = fetched(&[("lm", &["qwen", "llama", "phi"])]);
        let ids = list_model_ids(false, &entries, &fetched);
        assert_eq!(ids, vec!["lm/qwen", "lm/llama", "lm/phi"]);

        // No default model: only the fetched list appears.
        let entries = [entry("lm", "")];
        let ids = list_model_ids(false, &entries, &fetched);
        assert_eq!(ids, vec!["lm/qwen", "lm/llama", "lm/phi"]);
    }

    #[test]
    fn resolves_builtin_deepseek_models() {
        let resolved = resolve_model("deepseek-chat", true, &[], &none()).unwrap();
        assert_eq!(
            resolved,
            ResolvedModel::Deepseek {
                model: "deepseek-chat".into()
            }
        );
        // Case-insensitive, and reachable under the reserved prefix too.
        assert!(
            resolve_model("DeepSeek-Reasoner", true, &[], &none())
                .unwrap()
                .is_deepseek()
        );
        assert!(
            resolve_model("deepseek/deepseek-chat", true, &[], &none())
                .unwrap()
                .is_deepseek()
        );
        // Without a token the built-in backend does not exist.
        assert!(resolve_model("deepseek-chat", false, &[], &none()).is_err());
    }

    #[test]
    fn entry_prefix_overrides_the_configured_model() {
        let entries = [entry("lmstudio", "qwen")];
        let resolved = resolve_model("lmstudio/llama-3.3-70b", true, &entries, &none()).unwrap();
        match resolved {
            ResolvedModel::Entry { entry } => {
                assert_eq!(entry.name, "lmstudio");
                assert_eq!(entry.model, "llama-3.3-70b");
            }
            other => panic!("expected entry, got {other:?}"),
        }
        // Slashes inside the sub-model survive (OpenRouter-style ids).
        let resolved = resolve_model("router/qwen/qwen-2.5-coder", true, &entries, &none());
        assert!(resolved.is_err(), "no entry named 'router'");
        let entries = [entry("router", "x")];
        match resolve_model("router/qwen/qwen-2.5-coder", true, &entries, &none()).unwrap() {
            ResolvedModel::Entry { entry } => assert_eq!(entry.model, "qwen/qwen-2.5-coder"),
            other => panic!("expected entry, got {other:?}"),
        }
    }

    #[test]
    fn bare_entry_name_and_bare_configured_model_resolve() {
        let entries = [entry("lmstudio", "qwen")];
        match resolve_model("lmstudio", true, &entries, &none()).unwrap() {
            ResolvedModel::Entry { entry } => assert_eq!(entry.model, "qwen"),
            other => panic!("expected entry, got {other:?}"),
        }
        match resolve_model("qwen", true, &entries, &none()).unwrap() {
            ResolvedModel::Entry { entry } => assert_eq!(entry.name, "lmstudio"),
            other => panic!("expected entry, got {other:?}"),
        }
    }

    #[test]
    fn bare_fetched_model_resolves_to_its_entry() {
        let entries = [entry("lm", "qwen"), entry("router", "x")];
        let fetched = fetched(&[("router", &["deepseek/deepseek-r1", "moonshot/kimi-k2"])]);
        match resolve_model("moonshot/kimi-k2", false, &entries, &fetched).unwrap() {
            // `<prefix>/` didn't match an entry name, but the full id is in
            // router's fetched list — wait, it DOES contain '/' and no
            // entry is named "moonshot", so the fetched-list fallback wins.
            ResolvedModel::Entry { entry } => {
                assert_eq!(entry.name, "router");
                assert_eq!(entry.model, "moonshot/kimi-k2");
            }
            other => panic!("expected entry, got {other:?}"),
        }
        // Plain unprefixed fetched id, case-insensitive.
        match resolve_model("Kimi-K2", false, &entries, &fetched) {
            Err(_) => {} // not fetched under that exact id — stays a 404
            Ok(other) => panic!("expected not-found, got {other:?}"),
        }
    }

    #[test]
    fn bare_entry_without_default_uses_first_fetched_model() {
        let entries = [entry("lm", "")];
        let with_models = fetched(&[("lm", &["alpha", "beta"])]);
        match resolve_model("lm", false, &entries, &with_models).unwrap() {
            ResolvedModel::Entry { entry } => assert_eq!(entry.model, "alpha"),
            other => panic!("expected entry, got {other:?}"),
        }
        // No fetched models either → explicit error, not an empty model.
        let error = resolve_model("lm", false, &entries, &none()).unwrap_err();
        assert!(error.contains("no default model"), "got: {error}");
    }

    #[test]
    fn unknown_ids_report_not_found() {
        assert!(resolve_model("gpt-5", true, &[], &none()).is_err());
        assert!(resolve_model("", true, &[], &none()).is_err());
        assert!(resolve_model("deepseek/gpt-5", true, &[], &none()).is_err());
    }

    #[test]
    fn internal_model_reports_the_effective_model() {
        let entries = [entry("lmstudio", "qwen")];
        let resolved = resolve_model("lmstudio/other", true, &entries, &none()).unwrap();
        assert_eq!(resolved.internal_model(), "other");
        let resolved = resolve_model("deepseek-reasoner", true, &[], &none()).unwrap();
        assert_eq!(resolved.internal_model(), "deepseek-reasoner");
    }
}
