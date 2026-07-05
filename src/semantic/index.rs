//! Generic hybrid retrieval index shared by every semantic corpus (skills,
//! MCP tools): dense cosine over pre-embedded passages + stemmed sparse
//! TF-IDF, fused with Reciprocal Rank Fusion. Corpus-specific typing
//! (what a document *is*) lives in `matcher.rs`; this file only ranks.

use super::embedder::{dot, Embedder};
use super::sparse::SparseIndex;

/// RRF smoothing constant — the standard value from the original paper;
/// larger k flattens the contribution difference between adjacent ranks.
const RRF_K: f32 = 60.0;

/// One ranked document: corpus position plus the raw per-channel scores
/// (useful for logging and threshold calibration).
pub struct Hit {
    pub index: usize,
    pub dense: f32,
    pub sparse: f32,
}

pub struct HybridIndex {
    dense: Vec<Vec<f32>>,
    sparse: SparseIndex,
}

impl HybridIndex {
    /// Embed and index `texts`. Blocking ONNX inference — call from
    /// `spawn_blocking`.
    pub fn build(embedder: &mut Embedder, texts: &[String]) -> Result<Self, String> {
        Ok(Self {
            dense: embedder.embed_passages(texts)?,
            sparse: SparseIndex::build(texts),
        })
    }

    pub fn len(&self) -> usize {
        self.dense.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dense.is_empty()
    }

    /// Rank every document against an already-embedded query, best first.
    /// A document passes the gate when its dense cosine clears `min_dense`
    /// OR it has any lexical overlap with the prompt — that's what keeps
    /// suggestions quiet on small talk instead of always returning
    /// *something*. Callers apply their own filters and `take(top_k)`.
    pub fn query(&self, query_vec: &[f32], prompt: &str, min_dense: f32) -> Vec<Hit> {
        if self.is_empty() {
            return Vec::new();
        }
        let dense_scores: Vec<f32> = self.dense.iter().map(|d| dot(query_vec, d)).collect();
        let sparse_scores = self.sparse.scores(prompt);
        let fused = rrf_fuse(&[&dense_scores, &sparse_scores]);

        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_by(|&a, &b| fused[b].total_cmp(&fused[a]));

        order
            .into_iter()
            .filter(|&i| dense_scores[i] >= min_dense || sparse_scores[i] > 1e-6)
            .map(|i| Hit {
                index: i,
                dense: dense_scores[i],
                sparse: sparse_scores[i],
            })
            .collect()
    }
}

/// Reciprocal Rank Fusion over several score lists (all in corpus order).
/// Returns one fused score per document. Shared with the history store,
/// which owns its vectors (incremental appends) instead of a `HybridIndex`.
pub(super) fn rrf_fuse(score_lists: &[&Vec<f32>]) -> Vec<f32> {
    let n = score_lists.first().map_or(0, |l| l.len());
    let mut fused = vec![0.0f32; n];
    for scores in score_lists {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
        for (rank, &doc) in order.iter().enumerate() {
            // Documents with zero signal in this list shouldn't earn rank
            // credit just for existing.
            if scores[doc] > 1e-6 {
                fused[doc] += 1.0 / (RRF_K + rank as f32 + 1.0);
            }
        }
    }
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rewards_agreement_between_rankers() {
        let dense = vec![0.9, 0.5, 0.7];
        let sparse = vec![0.8, 0.0, 0.1];
        let fused = rrf_fuse(&[&dense, &sparse]);
        // Doc 0 is first in both lists — it must win.
        assert!(fused[0] > fused[2] && fused[2] > fused[1], "{fused:?}");
    }

    #[test]
    fn rrf_gives_no_credit_for_zero_scores() {
        let dense = vec![0.9, 0.0];
        let sparse = vec![0.0, 0.0];
        let fused = rrf_fuse(&[&dense, &sparse]);
        assert_eq!(fused[1], 0.0);
    }
}
