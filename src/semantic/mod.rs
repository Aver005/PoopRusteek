//! Local semantic matching ("RAG-lite") over the skill catalog.
//!
//! Embeds every discovered skill's name/description with a local ONNX
//! model (multilingual-e5-small via fastembed — downloaded once into
//! `Config::data_dir()/models`, fully offline afterwards) and matches each
//! outgoing user prompt against the corpus with a dense+sparse hybrid.
//! Matches surface as an advisory note appended to the turn, pointing the
//! model at the `skill` tool — the model stays in control; nothing is
//! auto-loaded.
//!
//! Threading contract: initialization and inference are CPU/IO-bound and
//! only ever run on `spawn_blocking` threads. `SemanticService` itself is a
//! cheap shared handle that answers instantly (with no matches) until the
//! background init completes.

pub mod embedder;
pub mod matcher;
pub mod sparse;

#[cfg(test)]
mod eval;

use crate::app::events::AppEvent;
use crate::config::Config;
use crate::skills::SkillDefinition;
use matcher::{SkillMatch, SkillMatcher};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

enum MatcherState {
    /// Disabled by config or no skills discovered — stays inert forever.
    Disabled,
    /// Background init (model download / load + corpus embedding) running.
    Initializing,
    /// Boxed: the matcher (ONNX session + corpus vectors) dwarfs the other
    /// variants and clippy rightly objects to carrying that inline.
    Ready(Box<SkillMatcher>),
    Failed,
}

pub struct SemanticService {
    state: Mutex<MatcherState>,
    top_k: usize,
    min_dense: f32,
}

impl SemanticService {
    /// Create the service and, when enabled, kick off background
    /// initialization. Cheap; never blocks the caller.
    pub fn start(
        config: &Config,
        skills: Vec<SkillDefinition>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Arc<Self> {
        let enabled = config.semantic.enabled && !skills.is_empty();
        let service = Arc::new(Self {
            state: Mutex::new(if enabled { MatcherState::Initializing } else { MatcherState::Disabled }),
            top_k: config.semantic.top_k,
            min_dense: config.semantic.min_dense_score,
        });
        if !enabled {
            return service;
        }

        let this = Arc::clone(&service);
        tokio::spawn(async move {
            let cache_dir = Config::data_dir().join("models");
            let first_run = !embedder::model_cache_present(&cache_dir);
            if first_run {
                let _ = event_tx.send(AppEvent::SemanticStatus(
                    "Downloading embedding model (~120 MB, one-time)...".to_string(),
                ));
            }

            let built =
                tokio::task::spawn_blocking(move || SkillMatcher::build(&skills, cache_dir)).await;

            match built {
                Ok(Ok(skill_matcher)) => {
                    let count = skill_matcher.len();
                    *this.state.lock().unwrap() = MatcherState::Ready(Box::new(skill_matcher));
                    tracing::info!("semantic: skill matcher ready ({count} skills)");
                    if first_run {
                        let _ = event_tx.send(AppEvent::SemanticStatus(
                            "Embedding model ready — skill matching active".to_string(),
                        ));
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("semantic: init failed: {e}");
                    *this.state.lock().unwrap() = MatcherState::Failed;
                    let _ = event_tx.send(AppEvent::SemanticStatus(format!(
                        "Skill matching unavailable: {e}"
                    )));
                }
                Err(join_err) => {
                    tracing::warn!("semantic: init task panicked: {join_err}");
                    *this.state.lock().unwrap() = MatcherState::Failed;
                }
            }
        });

        service
    }

    /// An always-off service for contexts with no config/event channel.
    #[cfg(test)]
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MatcherState::Disabled),
            top_k: 0,
            min_dense: 1.0,
        })
    }

    /// Match `prompt` against the skill corpus. Returns an empty list until
    /// background init completes. Blocking (ONNX inference under the state
    /// lock, no awaits) — call from `spawn_blocking`.
    pub fn match_skills(&self, prompt: &str) -> Vec<SkillMatch> {
        let mut guard = self.state.lock().unwrap();
        match &mut *guard {
            MatcherState::Ready(skill_matcher) => {
                skill_matcher.query(prompt, self.top_k, self.min_dense)
            }
            _ => Vec::new(),
        }
    }

    /// Render matches as the advisory block appended to an outgoing turn.
    /// Deliberately framed as optional — a false positive must be ignorable.
    pub fn render_hint(matches: &[SkillMatch]) -> String {
        let mut lines = vec![
            "[Skill hint — automatic semantic match, may be irrelevant]".to_string(),
            "Skills that may apply to the user's request:".to_string(),
        ];
        for m in matches {
            lines.push(format!("- {} — {}", m.slug, m.description));
        }
        lines.push(
            "If one clearly fits, load it with the `skill` tool (action=\"load\", name=\"<slug>\") before answering. Otherwise ignore this note."
                .to_string(),
        );
        lines.join("\n")
    }
}
