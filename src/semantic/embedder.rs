//! Thin wrapper around fastembed's `TextEmbedding`, pinned to
//! multilingual-e5-small (384-dim, quantized ONNX, ~120 MB one-time
//! download). Enforces the e5 asymmetric-prefix contract — documents are
//! embedded as `passage: …`, search inputs as `query: …`; mixing them up
//! costs double-digit recall — and L2-normalizes every output so a plain
//! dot product equals cosine similarity.

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::path::{Path, PathBuf};

/// Identity of the pinned model — persisted indexes (message history)
/// stamp these into their header and rebuild when they change.
pub const MODEL_ID: &str = "intfloat/multilingual-e5-small";
pub const EMBEDDING_DIM: usize = 384;

pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Load the model, downloading it into `cache_dir` on first run.
    /// Blocking (network + disk + ONNX session build) — call from
    /// `spawn_blocking`, never on the event loop.
    pub fn init(cache_dir: PathBuf) -> Result<Self, String> {
        // ONNX Runtime defaults its intra-op pool to EVERY core, so the
        // post-launch indexing burst (skills + MCP corpus + history
        // backfill) saturated the whole machine for tens of seconds and
        // starved the TUI event loop — felt as hard input lag right after
        // startup. Cap it to half the cores (1..=4): background indexing
        // merely takes longer, and per-turn single-query embeddings stay
        // in the low milliseconds either way.
        let intra_threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, 4))
            .unwrap_or(1);
        let options = TextInitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_cache_dir(cache_dir)
            // fastembed's progress bar writes to stdout, which would corrupt
            // the ratatui frame — download status is surfaced via AppEvent
            // by the caller instead.
            .with_show_download_progress(false)
            .with_intra_threads(intra_threads);
        let model = TextEmbedding::try_new(options)
            .map_err(|e| format!("embedding model init failed: {e}"))?;
        Ok(Self { model })
    }

    /// Embed a search input (`query: ` prefix). Blocking ONNX inference.
    pub fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let mut out = self.embed_with_prefix("query: ", std::slice::from_ref(&text.to_string()))?;
        out.pop()
            .ok_or_else(|| "embedder returned no vector".to_string())
    }

    /// Embed corpus documents (`passage: ` prefix). Blocking ONNX inference.
    pub fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        self.embed_with_prefix("passage: ", texts)
    }

    fn embed_with_prefix(
        &mut self,
        prefix: &str,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        let mut vectors = self
            .model
            .embed(prefixed, None)
            .map_err(|e| format!("embedding failed: {e}"))?;
        for v in &mut vectors {
            l2_normalize(v);
        }
        Ok(vectors)
    }
}

/// True when the model cache already holds files — used to decide whether
/// to warn the user about the one-time download before init starts.
pub fn model_cache_present(cache_dir: &Path) -> bool {
    std::fs::read_dir(cache_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Dot product; on L2-normalized inputs this is cosine similarity.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_produces_unit_vector() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_leaves_zero_vector_untouched() {
        let mut v = vec![0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0]);
    }
}
