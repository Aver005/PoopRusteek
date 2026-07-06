//! Persistent cache of each `/providers` entry's model list (its
//! `GET /models`). Feeds the API server's `/v1/models` + model-id routing
//! and the `/serve` status display, so the gateway exposes *every* model an
//! upstream serves — not just the entry's configured default.
//!
//! Freshness knobs live in `[provider_models]` (`config`):
//! - `cache_ms` — how long the persisted list stays valid across restarts
//!   (0 = always re-fetch on startup);
//! - `refetch_ms` — background re-fetch period while running (0 = only on
//!   startup / provider add).
//!
//! The in-memory map is behind a plain `std::sync::Mutex` (tiny, held for
//! synchronous map ops only — never across an `.await`); network fetches
//! run outside the lock and merge afterwards. Persistence goes through
//! `util::atomic_write` like every other user-data file.

use crate::config::ProviderEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

/// Upstream `GET /models` calls are bounded so one dead endpoint can't
/// stall a whole refresh round (providers keep their own 10s connect
/// timeout; this also caps slow-but-alive responders).
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModels {
    models: Vec<String>,
    /// Unix ms of the successful fetch.
    fetched_at_ms: u64,
}

/// On-disk shape (`data_dir/provider_models.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    providers: HashMap<String, CachedModels>,
}

pub struct ProviderModelCache {
    path: PathBuf,
    inner: Mutex<HashMap<String, CachedModels>>,
}

/// One refresh round's outcome, for status lines and the proxy log.
#[derive(Debug, Default, Clone)]
pub struct RefreshOutcome {
    /// `(entry name, model count)` per successful fetch.
    pub fetched: Vec<(String, usize)>,
    /// `(entry name, error)` per failure (stale cache, if any, is kept).
    pub failed: Vec<(String, String)>,
    /// Entries skipped because their cache was still fresh.
    pub skipped: usize,
}

impl RefreshOutcome {
    /// One-line human summary ("models: lmstudio 14, router 92; ollama failed").
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = self
            .fetched
            .iter()
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        if !self.failed.is_empty() {
            parts.push(format!(
                "{} failed",
                self.failed
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            ));
        }
        if self.skipped > 0 {
            parts.push(format!("{} cached", self.skipped));
        }
        if parts.is_empty() {
            "no providers to fetch".to_string()
        } else {
            format!("provider models: {}", parts.join(", "))
        }
    }
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

impl ProviderModelCache {
    pub fn default_path() -> PathBuf {
        crate::config::Config::data_dir().join("provider_models.json")
    }

    /// Load the persisted cache (missing/corrupt file = empty cache — it
    /// is rebuildable by construction).
    pub fn load() -> Arc<Self> {
        let path = Self::default_path();
        let providers = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok())
            .map(|file| file.providers)
            .unwrap_or_default();
        Arc::new(Self { path, inner: Mutex::new(providers) })
    }

    #[cfg(test)]
    pub fn empty_for_tests() -> Arc<Self> {
        Arc::new(Self {
            // Never persisted: `store` targets this path only via refresh,
            // and tests point it into a scratch dir.
            path: std::env::temp_dir().join("pooprusteek_test_provider_models.json"),
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Current `name → models` view for the catalog. Cheap clone — model
    /// lists are short strings.
    pub fn snapshot(&self) -> HashMap<String, Vec<String>> {
        self.inner
            .lock()
            .map(|map| {
                map.iter()
                    .map(|(name, cached)| (name.clone(), cached.models.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Age of one entry's cached list, if present.
    pub fn age_ms(&self, name: &str) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|map| map.get(name).map(|cached| now_ms().saturating_sub(cached.fetched_at_ms)))
    }

    fn is_fresh(&self, name: &str, cache_ttl_ms: u64) -> bool {
        cache_ttl_ms > 0 && self.age_ms(name).is_some_and(|age| age < cache_ttl_ms)
    }

    fn store(&self, name: &str, models: Vec<String>) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(name.to_string(), CachedModels { models, fetched_at_ms: now_ms() });
        }
        self.persist();
    }

    /// Drop cache rows for providers that no longer exist in the config.
    fn retain_known(&self, known: &[ProviderEntry]) {
        if let Ok(mut map) = self.inner.lock() {
            map.retain(|name, _| known.iter().any(|entry| &entry.name == name));
        }
    }

    fn persist(&self) {
        let file = CacheFile {
            version: 1,
            providers: self.inner.lock().map(|map| map.clone()).unwrap_or_default(),
        };
        match serde_json::to_vec_pretty(&file) {
            Ok(bytes) => {
                if let Err(error) = crate::util::atomic_write(&self.path, &bytes) {
                    tracing::warn!("provider model cache: persist failed: {error}");
                }
            }
            Err(error) => tracing::warn!("provider model cache: serialize failed: {error}"),
        }
    }

    /// Fetch model lists for `entries`, concurrently. Entries whose cache
    /// is younger than `cache_ttl_ms` are skipped unless `force` (periodic
    /// refetch and provider-add use force; startup respects the cache).
    /// Failures keep whatever was cached before. Never panics, never holds
    /// the lock across an await.
    pub async fn refresh(
        self: &Arc<Self>,
        entries: &[ProviderEntry],
        cache_ttl_ms: u64,
        force: bool,
    ) -> RefreshOutcome {
        self.retain_known(entries);
        let mut outcome = RefreshOutcome::default();
        let mut jobs = Vec::new();
        for entry in entries {
            if !force && self.is_fresh(&entry.name, cache_ttl_ms) {
                outcome.skipped += 1;
                continue;
            }
            let entry = entry.clone();
            jobs.push(async move {
                let result = match crate::provider::build_entry_provider(&entry) {
                    Ok(provider) => {
                        match tokio::time::timeout(FETCH_TIMEOUT, provider.list_models()).await {
                            Ok(Ok(models)) => Ok(models),
                            Ok(Err(error)) => Err(error.to_string()),
                            Err(_) => Err(format!("timed out after {}s", FETCH_TIMEOUT.as_secs())),
                        }
                    }
                    Err(error) => Err(error.to_string()),
                };
                (entry.name, result)
            });
        }
        for (name, result) in futures::future::join_all(jobs).await {
            match result {
                Ok(models) => {
                    outcome.fetched.push((name.clone(), models.len()));
                    self.store(&name, models);
                }
                Err(error) => {
                    tracing::debug!("provider model cache: '{name}' fetch failed: {error}");
                    outcome.failed.push((name, error));
                }
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProtocol;

    fn entry(name: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            base_url: "http://127.0.0.1:9/v1".to_string(), // port 9: nothing listens
            api_key: None,
            model: "m".to_string(),
            protocol: ProviderProtocol::Openai,
        }
    }

    #[test]
    fn snapshot_and_freshness_reflect_stores() {
        let cache = ProviderModelCache::empty_for_tests();
        assert!(cache.snapshot().is_empty());
        assert!(!cache.is_fresh("lm", 60_000));

        // Insert directly (bypassing persist-to-temp is fine — same path).
        cache.inner.lock().unwrap().insert(
            "lm".to_string(),
            CachedModels { models: vec!["a".into(), "b".into()], fetched_at_ms: now_ms() },
        );
        assert_eq!(cache.snapshot().get("lm").map(Vec::len), Some(2));
        assert!(cache.is_fresh("lm", 60_000));
        // TTL 0 = cache disabled: nothing is ever fresh.
        assert!(!cache.is_fresh("lm", 0));
    }

    #[tokio::test]
    async fn refresh_skips_fresh_entries_and_reports_failures() {
        let cache = ProviderModelCache::empty_for_tests();
        cache.inner.lock().unwrap().insert(
            "cached".to_string(),
            CachedModels { models: vec!["kept".into()], fetched_at_ms: now_ms() },
        );

        // "cached" is fresh → skipped; "dead" points at a closed port → fails.
        let outcome = cache
            .refresh(&[entry("cached"), entry("dead")], 60_000, false)
            .await;
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.fetched.len(), 0);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, "dead");
        // The stale-keeping rule: a failure must not wipe existing models.
        assert_eq!(cache.snapshot().get("cached").map(Vec::len), Some(1));

        // Rows for providers deleted from the config are pruned.
        cache.refresh(&[entry("dead")], 60_000, false).await;
        assert!(cache.snapshot().get("cached").is_none());
    }

    #[test]
    fn outcome_summary_reads_well() {
        let outcome = RefreshOutcome {
            fetched: vec![("lm".into(), 14)],
            failed: vec![("ollama".into(), "boom".into())],
            skipped: 2,
        };
        assert_eq!(outcome.summary(), "provider models: lm 14, ollama failed, 2 cached");
        assert_eq!(RefreshOutcome::default().summary(), "no providers to fetch");
    }
}
