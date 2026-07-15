//! Local semantic matching ("RAG-lite") over the skill catalog and the MCP
//! tool catalog.
//!
//! Embeds both corpora with a local ONNX model (multilingual-e5-small via
//! fastembed — downloaded once into `Config::data_dir()/models`, fully
//! offline afterwards) and matches each outgoing user prompt against them
//! with a dense+sparse hybrid. Matches surface as an advisory note
//! appended to the turn — the model stays in control; nothing is
//! auto-loaded. The same index also backs the `tool_search` builtin tool,
//! which is what makes deferred MCP schemas (compact tool list in the
//! system prompt, full definitions on demand) safe: a tool the matcher
//! didn't volunteer can still be looked up explicitly.
//!
//! Threading contract: initialization and inference are CPU/IO-bound and
//! only ever run on `spawn_blocking` threads. `SemanticService` itself is a
//! cheap shared handle that answers instantly (with nothing) until the
//! background init completes. The inner mutex is only ever held across
//! synchronous work — never across an `.await`.

pub mod embedder;
pub mod history;
pub mod index;
pub mod matcher;
pub mod sparse;

#[cfg(test)]
mod eval;

use crate::app::events::AppEvent;
use crate::config::{Config, RagLimit, SemanticConfig};
use crate::mcp::manager::FullMCPTool;
use crate::skills::SkillDefinition;
use matcher::{McpCorpus, McpToolMatch, SkillCorpus, SkillMatch};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long a per-turn hint may wait for the index lock before the turn
/// proceeds without one. Long enough to ride out another turn's query
/// embedding (a few ms), far too short to wait out an indexing hold
/// (corpus rebuild / history backfill run for seconds to minutes).
const HINT_LOCK_BUDGET: Duration = Duration::from_millis(150);

/// Readiness is implicit: `inner.embedder.is_some()` means embeddings are
/// live. Before that (still initializing, init failed, or disabled by
/// config) every matching entry point degrades gracefully — empty matches,
/// lexical `tool_search` fallback — so no explicit phase tracking is
/// needed.
#[derive(Default)]
struct Inner {
    embedder: Option<Box<embedder::Embedder>>,
    skills: Option<SkillCorpus>,
    mcp: Option<McpCorpus>,
    /// Latest raw MCP tool list — kept even when embeddings are
    /// unavailable so `tool_search` can fall back to lexical matching,
    /// and so tools that arrive mid-init get embedded once init lands.
    raw_mcp: Vec<FullMCPTool>,
    /// Persistent message-history index (loaded/backfilled during init).
    history: Option<history::HistoryStore>,
}

/// Everything a turn hint can carry: skill suggestions + MCP tool
/// suggestions, matched against one shared query embedding.
#[derive(Default)]
pub struct PromptMatches {
    pub skills: Vec<SkillMatch>,
    pub mcp_tools: Vec<McpToolMatch>,
}

impl PromptMatches {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.mcp_tools.is_empty()
    }
}

/// Point-in-time state for `/rag` status display.
pub struct RagStatus {
    pub enabled: bool,
    pub ready: bool,
    pub init_running: bool,
    pub model_on_disk: bool,
    pub skills_indexed: usize,
    pub mcp_indexed: usize,
    pub mcp_known: usize,
    pub deferred: bool,
    pub history_chunks: usize,
    pub history_sessions: usize,
}

pub struct SemanticService {
    inner: Mutex<Inner>,
    config: SemanticConfig,
    /// Runtime switch behind `/rag on|off` — starts from `config.enabled`.
    /// Off means: no hints, no semantic ranking (lexical `tool_search`
    /// fallback stays), and the app inlines full MCP schemas again.
    enabled: AtomicBool,
    /// Guard so `/rag on` + `/rag reload` can't stack duplicate inits.
    init_running: AtomicBool,
    /// Resolved on every `update_mcp_tools`: whether the current tool count
    /// puts the system prompt in deferred-schema mode. Hints mirror this —
    /// in deferred mode a matched tool's full definition rides in the hint,
    /// because the system prompt no longer carries it.
    mcp_deferred: AtomicBool,
    /// Resolved embedder batch cap (fastembed's `embed` batch arg). `0` is
    /// the sentinel for "no cap" (`None`). Set from `[semantic] rag_limit` +
    /// detected RAM at `start`, updated live by `/rag-limit`, and re-read by
    /// `spawn_init` so a reload keeps the current setting.
    embed_batch: AtomicUsize,
}

impl SemanticService {
    /// Create the service and, when enabled, kick off background
    /// initialization. Cheap; never blocks the caller.
    pub fn start(
        config: &Config,
        skills: Vec<SkillDefinition>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Arc<Self> {
        let batch = config
            .semantic
            .rag_limit
            .resolve(crate::util::total_ram_bytes());
        let service = Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            config: config.semantic.clone(),
            enabled: AtomicBool::new(config.semantic.enabled),
            init_running: AtomicBool::new(false),
            mcp_deferred: AtomicBool::new(false),
            embed_batch: AtomicUsize::new(batch.unwrap_or(0)),
        });
        if config.semantic.enabled {
            service.spawn_init(skills, event_tx);
        }
        service
    }

    /// Launch model load (+ download on first run) and corpus embedding on
    /// a background task. No-op if an init is already in flight.
    pub fn spawn_init(
        self: &Arc<Self>,
        skills: Vec<SkillDefinition>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) {
        if self.init_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let cache_dir = Config::data_dir().join("models");
            let first_run = !embedder::model_cache_present(&cache_dir);
            if first_run {
                let _ = event_tx.send(AppEvent::SemanticStatus(
                    "Downloading embedding model (~120 MB, one-time)...".to_string(),
                ));
            }

            let this_blocking = Arc::clone(&this);
            let built = tokio::task::spawn_blocking(move || {
                let mut emb = embedder::Embedder::init(cache_dir)?;
                emb.set_batch_limit(this_blocking.resolved_batch());
                let skill_corpus = SkillCorpus::build(&mut emb, &skills)?;
                let history_store = history::HistoryStore::open(Self::history_path());
                // Store under the lock, then embed any MCP tools that
                // arrived while the model was loading — same thread, same
                // blocking budget. The session backfill runs last: it can
                // be the slowest part (first run embeds every saved
                // session) and everything else is already usable.
                {
                    let mut inner = this_blocking.lock_inner();
                    inner.embedder = Some(Box::new(emb));
                    inner.skills = Some(skill_corpus);
                    inner.history = Some(history_store);
                }
                this_blocking.rebuild_mcp_corpus();
                this_blocking.backfill_history();
                Ok::<usize, String>(
                    this_blocking
                        .lock_inner()
                        .skills
                        .as_ref()
                        .map_or(0, SkillCorpus::len),
                )
            })
            .await;

            this.init_running.store(false, Ordering::SeqCst);
            match built {
                Ok(Ok(count)) => {
                    tracing::info!("semantic: matcher ready ({count} skills)");
                    if first_run {
                        let _ = event_tx.send(AppEvent::SemanticStatus(
                            "Embedding model ready — semantic matching active".to_string(),
                        ));
                    }
                }
                Ok(Err(e)) => {
                    // error! (not warn!) so it surfaces the red UI marker —
                    // a dead embedder is exactly what the user should notice.
                    tracing::error!("semantic: init failed: {e}");
                    let _ = event_tx.send(AppEvent::SemanticStatus(format!(
                        "Semantic matching unavailable: {e}"
                    )));
                }
                Err(join_err) => {
                    tracing::warn!("semantic: init task panicked: {join_err}");
                }
            }
        });
    }

    /// `/rag on|off`. Recomputes deferred-schema state for the current tool
    /// count; the caller is responsible for persisting the config flag and
    /// (on enable) calling `spawn_init` if `is_ready()` is still false.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        let tool_count = self.lock_inner().raw_mcp.len();
        self.mcp_deferred.store(
            on && self.config.mcp_schemas.deferred_for(tool_count),
            Ordering::Relaxed,
        );
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The resolved embedder batch cap in effect (`None` = no cap). Backs
    /// `spawn_init` (so reloads keep the setting) and `/rag-limit` status.
    pub fn resolved_batch(&self) -> Option<usize> {
        let v = self.embed_batch.load(Ordering::Relaxed);
        (v != 0).then_some(v)
    }

    /// `/rag-limit <n|auto|off>` — resolve `limit` against current RAM, store
    /// it for future (re)inits, and apply it to the live embedder if one is
    /// loaded. Returns the concrete batch that took effect (`None` = no cap).
    pub fn set_embed_batch(&self, limit: RagLimit) -> Option<usize> {
        let resolved = limit.resolve(crate::util::total_ram_bytes());
        self.embed_batch
            .store(resolved.unwrap_or(0), Ordering::Relaxed);
        if let Some(emb) = self.lock_inner().embedder.as_mut() {
            emb.set_batch_limit(resolved);
        }
        resolved
    }

    pub fn is_ready(&self) -> bool {
        self.lock_inner().embedder.is_some()
    }

    /// `/rag reload` — drop the loaded model and every embedded corpus,
    /// then re-init from scratch: the model files are re-verified (missing
    /// ones re-downloaded) and skills re-embedded; the kept `raw_mcp` list
    /// re-embeds automatically when init lands. No-op if an init is
    /// already in flight (returns false so the caller can say so).
    pub fn reload(
        self: &Arc<Self>,
        skills: Vec<SkillDefinition>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> bool {
        if self.init_running.load(Ordering::SeqCst) {
            return false;
        }
        {
            let mut inner = self.lock_inner();
            inner.embedder = None;
            inner.skills = None;
            inner.mcp = None;
            // Dropped from memory only — the persisted file is re-opened by
            // init (and wiped there if the model stamp no longer matches),
            // then the session backfill re-indexes anything missing.
            inner.history = None;
        }
        self.spawn_init(skills, event_tx);
        true
    }

    /// Acquire the inner lock without unbounded waiting. `budget == ZERO`
    /// means exactly one `try_lock` (safe from the event loop); a positive
    /// budget retries with short sleeps (blocking threads only). Returns
    /// `None` on timeout or a poisoned lock — every caller treats that as
    /// "index busy, degrade" per the semantic-layer contract.
    fn lock_inner_bounded(&self, budget: Duration) -> Option<MutexGuard<'_, Inner>> {
        let deadline = Instant::now() + budget;
        loop {
            match self.inner.try_lock() {
                Ok(guard) => return Some(guard),
                Err(TryLockError::Poisoned(_)) => return None,
                Err(TryLockError::WouldBlock) => {}
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Poison-tolerant unbounded lock for blocking-thread paths. A panic on
    /// an embedding thread must not take the whole semantic layer down with
    /// it (the layer's contract: degrade, never crash) — recover the inner
    /// state and continue. Safe because every write path replaces fields or
    /// rebuilds a corpus wholesale, so post-poison state is at worst stale,
    /// never half-applied in a way the next rebuild wouldn't overwrite.
    /// Event-loop paths use `lock_inner_bounded`, which instead treats
    /// poison as "busy, degrade".
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Point-in-time state for `/rag`. Returns `None` while an indexing
    /// pass holds the index lock — the caller runs on the event loop and
    /// must not wait out a multi-second (post-thread-cap: longer) hold.
    pub fn status(&self) -> Option<RagStatus> {
        let inner = self.lock_inner_bounded(Duration::ZERO)?;
        Some(RagStatus {
            enabled: self.is_enabled(),
            ready: inner.embedder.is_some(),
            init_running: self.init_running.load(Ordering::SeqCst),
            model_on_disk: embedder::model_cache_present(&Config::data_dir().join("models")),
            skills_indexed: inner.skills.as_ref().map_or(0, SkillCorpus::len),
            mcp_indexed: inner.mcp.as_ref().map_or(0, McpCorpus::len),
            mcp_known: inner.raw_mcp.len(),
            deferred: self.mcp_deferred.load(Ordering::Relaxed),
            history_chunks: inner
                .history
                .as_ref()
                .map_or(0, history::HistoryStore::record_count),
            history_sessions: inner
                .history
                .as_ref()
                .map_or(0, history::HistoryStore::session_count),
        })
    }

    /// An always-off service for contexts with no config/event channel.
    #[cfg(test)]
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            config: SemanticConfig::default(),
            enabled: AtomicBool::new(false),
            init_running: AtomicBool::new(false),
            mcp_deferred: AtomicBool::new(false),
            embed_batch: AtomicUsize::new(0),
        })
    }

    /// Replace the MCP tool catalog (server connected / reloaded / toggled).
    /// Stores the raw list immediately; embedding happens on a blocking
    /// thread when the embedder is available, or is picked up by init when
    /// it lands. Cheap to call from the event loop.
    pub fn update_mcp_tools(self: &Arc<Self>, tools: Vec<FullMCPTool>) {
        self.mcp_deferred.store(
            self.is_enabled() && self.config.mcp_schemas.deferred_for(tools.len()),
            Ordering::Relaxed,
        );
        let embedder_ready = {
            let mut inner = self.lock_inner();
            inner.raw_mcp = tools;
            inner.embedder.is_some()
        };
        if !embedder_ready {
            return; // init completion (or nothing, if disabled/failed) picks it up
        }
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.rebuild_mcp_corpus());
    }

    /// Whether the system prompt should currently defer MCP schemas.
    /// `system_prompt::build` resolves the mode itself from the same
    /// config + live tool count; this accessor is for hint rendering.
    pub fn mcp_schemas_deferred(&self) -> bool {
        self.mcp_deferred.load(Ordering::Relaxed)
    }

    fn history_path() -> std::path::PathBuf {
        Config::data_dir().join("semantic").join("history.json")
    }

    /// Index every saved session the history store hasn't fully consumed
    /// yet. Blocking (embedding + file reads) — runs at the tail of init.
    /// Sessions whose on-disk message count matches the watermark are
    /// skipped without loading the file body.
    fn backfill_history(&self) {
        let summaries = match crate::session::list_sessions() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("semantic: session list for backfill failed: {e}");
                return;
            }
        };
        let mut indexed_sessions = 0usize;
        let mut indexed_chunks = 0usize;
        for summary in &summaries {
            {
                let inner = self.lock_inner();
                let Some(store) = inner.history.as_ref() else {
                    return;
                };
                if store.watermark(&summary.id) == summary.message_count {
                    continue;
                }
            }
            // `load_local` doesn't read its config parameter (kept for
            // signature stability) — a default is fine here.
            let Ok(session) = crate::session::load_local(&summary.id, &Config::default()) else {
                continue;
            };
            let mut inner = self.lock_inner();
            let Inner {
                embedder, history, ..
            } = &mut *inner;
            let (Some(emb), Some(store)) = (embedder, history) else {
                return;
            };
            match store.index_session(emb, &session.id, &summary.title, &session.messages) {
                Ok(0) => {}
                Ok(n) => {
                    indexed_sessions += 1;
                    indexed_chunks += n;
                }
                Err(e) => tracing::warn!("semantic: backfill of {} failed: {e}", session.id),
            }
        }
        if indexed_chunks > 0 {
            tracing::info!(
                "semantic: history backfill indexed {indexed_chunks} chunks from {indexed_sessions} sessions"
            );
        }
    }

    /// Index a just-saved session's new messages. Cheap to call from the
    /// event loop — the work happens on a blocking thread; skipped
    /// silently while disabled or before init lands (the startup backfill
    /// catches up later from the session file).
    pub fn index_session(
        self: &Arc<Self>,
        session_id: String,
        title: String,
        messages: Vec<crate::provider::ChatMessage>,
    ) {
        if !self.is_enabled() {
            return;
        }
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut inner = this.lock_inner();
            let Inner {
                embedder, history, ..
            } = &mut *inner;
            let (Some(emb), Some(store)) = (embedder, history) else {
                return;
            };
            if let Err(e) = store.index_session(emb, &session_id, &title, &messages) {
                tracing::warn!("semantic: indexing session {session_id} failed: {e}");
            }
        });
    }

    /// Search the message-history index. Returns an empty list while
    /// disabled or before init lands. Blocking — call from
    /// `spawn_blocking`.
    pub fn search_history(&self, query: &str, limit: usize) -> Vec<history::HistoryMatch> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let mut inner = self.lock_inner();
        let Inner {
            embedder, history, ..
        } = &mut *inner;
        let (Some(emb), Some(store)) = (embedder, history) else {
            return Vec::new();
        };
        match store.search(emb, query, limit) {
            Ok(matches) => matches,
            Err(e) => {
                tracing::warn!("semantic: history search failed: {e}");
                Vec::new()
            }
        }
    }

    /// (Re)embed the MCP corpus from `raw_mcp`. Blocking; holds the inner
    /// lock for the duration (seconds for dozens of tools, and the
    /// intra-thread cap trades speed for UI headroom) — a concurrent
    /// `match_prompt` gives up after its bounded wait and the turn simply
    /// goes out without a hint.
    fn rebuild_mcp_corpus(&self) {
        let mut inner = self.lock_inner();
        let Inner {
            embedder,
            mcp,
            raw_mcp,
            ..
        } = &mut *inner;
        let Some(emb) = embedder else { return };
        if raw_mcp.is_empty() {
            *mcp = None;
            return;
        }
        match McpCorpus::build(emb, raw_mcp) {
            Ok(corpus) => {
                tracing::info!("semantic: MCP tool corpus ready ({} tools)", corpus.len());
                *mcp = Some(corpus);
            }
            Err(e) => tracing::warn!("semantic: MCP corpus build failed: {e}"),
        }
    }

    /// Match `prompt` against both corpora with one query embedding.
    /// Returns nothing until background init completes, while `/rag off`
    /// is in effect, or when an indexing pass holds the index lock past
    /// [`HINT_LOCK_BUDGET`] — the hint is advisory and must never delay a
    /// turn's first token by the length of a corpus rebuild or history
    /// backfill. Blocking (ONNX inference under the state lock, plus the
    /// bounded lock wait) — call from `spawn_blocking`.
    pub fn match_prompt(&self, prompt: &str) -> PromptMatches {
        if !self.is_enabled() {
            return PromptMatches::default();
        }
        let Some(mut inner) = self.lock_inner_bounded(HINT_LOCK_BUDGET) else {
            tracing::debug!("semantic: hint skipped — index lock busy (indexing in progress)");
            return PromptMatches::default();
        };
        let Inner {
            embedder,
            skills,
            mcp,
            ..
        } = &mut *inner;
        let Some(emb) = embedder else {
            return PromptMatches::default();
        };
        let query_vec = match emb.embed_query(prompt) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("semantic: query embedding failed: {e}");
                return PromptMatches::default();
            }
        };
        let top_k = self.config.top_k;
        let min_dense = self.config.min_dense_score;
        PromptMatches {
            skills: skills
                .as_ref()
                .map(|c| c.query(&query_vec, prompt, top_k, min_dense))
                .unwrap_or_default(),
            mcp_tools: mcp
                .as_ref()
                .map(|c| c.query(&query_vec, prompt, top_k, min_dense))
                .unwrap_or_default(),
        }
    }

    /// Explicit MCP tool lookup for the `tool_search` builtin. Unlike the
    /// per-turn hint this is best-effort: no dense floor (the model asked,
    /// so return the closest thing we have), and when embeddings are
    /// unavailable it degrades to lexical substring matching over the raw
    /// tool list instead of returning nothing. Blocking — call from
    /// `spawn_blocking`.
    pub fn search_tools(&self, query: &str, limit: usize) -> Vec<McpToolMatch> {
        let mut inner = self.lock_inner();
        let Inner {
            embedder,
            mcp,
            raw_mcp,
            ..
        } = &mut *inner;

        // Semantic path only while enabled — `/rag off` means off; the
        // lexical fallback below stays as the dumb-but-working baseline.
        if self.is_enabled()
            && let (Some(emb), Some(corpus)) = (embedder.as_mut(), mcp.as_ref())
            && let Ok(query_vec) = emb.embed_query(query)
        {
            return corpus.query(&query_vec, query, limit, f32::MIN);
        }

        // Lexical fallback: rank by how many query words hit name+description.
        let needles: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 2)
            .map(str::to_string)
            .collect();
        if needles.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, &FullMCPTool)> = raw_mcp
            .iter()
            .map(|t| {
                let haystack = format!("{} {}", t.full_name, t.tool.description).to_lowercase();
                let hits = needles
                    .iter()
                    .filter(|n| haystack.contains(n.as_str()))
                    .count();
                (hits, t)
            })
            .filter(|(hits, _)| *hits > 0)
            .collect();
        scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, t)| McpToolMatch {
                full_name: t.full_name.clone(),
                description: t.tool.description.clone(),
                schema: t.tool.input_schema.clone(),
                dense: 0.0,
                sparse: 0.0,
            })
            .collect()
    }

    /// Render matches as the advisory block appended to an outgoing turn,
    /// or `None` when there is nothing to say. Deliberately framed as
    /// optional — a false positive must be ignorable. In deferred-schema
    /// mode matched MCP tools carry their full definition, because the
    /// system prompt no longer does.
    pub fn render_hint(&self, matches: &PromptMatches) -> Option<String> {
        if matches.is_empty() {
            return None;
        }
        let mut lines = vec!["[Hint — automatic semantic match, may be irrelevant]".to_string()];
        if !matches.skills.is_empty() {
            lines.push("Skills that may apply to the user's request:".to_string());
            for m in &matches.skills {
                lines.push(format!("- {} — {}", m.slug, m.description));
            }
            lines.push(
                "If one clearly fits, load it with the `skill` tool (action=\"load\", name=\"<slug>\") before answering."
                    .to_string(),
            );
        }
        if !matches.mcp_tools.is_empty() {
            lines.push("MCP tools that may apply:".to_string());
            let deferred = self.mcp_schemas_deferred();
            for m in &matches.mcp_tools {
                if deferred {
                    lines.push(crate::util::format_tool_definition(
                        &m.full_name,
                        &m.description,
                        &m.schema,
                    ));
                } else {
                    lines.push(format!("- `{}`: {}", m.full_name, m.description));
                }
            }
        }
        lines.push("If none of this fits the task, ignore this note.".to_string());
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    fn enabled_service_without_init() -> Arc<SemanticService> {
        Arc::new(SemanticService {
            inner: Mutex::new(Inner::default()),
            config: SemanticConfig::default(),
            enabled: AtomicBool::new(true),
            init_running: AtomicBool::new(false),
            mcp_deferred: AtomicBool::new(false),
            embed_batch: AtomicUsize::new(0),
        })
    }

    /// A held index lock (corpus rebuild, history backfill) must degrade,
    /// never block: `status()` declines immediately (it runs on the event
    /// loop) and `match_prompt` gives up within its bounded budget instead
    /// of delaying the turn's first token.
    #[test]
    fn busy_index_lock_degrades_instead_of_blocking() {
        let service = enabled_service_without_init();
        let guard = service.inner.lock().unwrap();

        assert!(
            service.status().is_none(),
            "status must decline while the lock is held"
        );

        let start = Instant::now();
        let matches = service.match_prompt("anything");
        assert!(matches.is_empty(), "no hint while the lock is held");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "match_prompt must give up within its bounded budget, waited {:?}",
            start.elapsed()
        );

        drop(guard);
        let status = service.status().expect("status once the lock is free");
        assert!(
            !status.ready,
            "no embedder was ever initialized in this test"
        );
    }

    /// A panic on an embedding thread poisons `inner`; blocking-path locks
    /// must recover instead of propagating the panic (degrade, never crash),
    /// while bounded event-loop paths keep treating poison as "busy".
    #[test]
    fn poisoned_lock_recovers_instead_of_panicking() {
        let service = enabled_service_without_init();
        let s2 = Arc::clone(&service);
        let _ = std::thread::spawn(move || {
            let _guard = s2.inner.lock().unwrap();
            panic!("simulated embedder panic");
        })
        .join();

        assert!(!service.is_ready(), "is_ready must answer, not panic");
        assert!(
            service.status().is_none(),
            "bounded event-loop path still declines on poison"
        );
    }
}
