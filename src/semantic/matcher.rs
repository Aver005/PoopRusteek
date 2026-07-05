//! Hybrid skill matcher: dense cosine (multilingual-e5-small) + stemmed
//! sparse TF-IDF, fused with Reciprocal Rank Fusion. Dense catches synonyms
//! and cross-language matches (Russian prompt → English skill description);
//! sparse catches exact keyword hits (`tailwind`, `playwright`, …) that
//! short descriptions can under-represent semantically.

use super::embedder::{dot, Embedder};
use super::sparse::SparseIndex;
use crate::skills::SkillDefinition;
use std::path::PathBuf;

/// RRF smoothing constant — the standard value from the original paper;
/// larger k flattens the contribution difference between adjacent ranks.
const RRF_K: f32 = 60.0;

#[derive(Debug, Clone)]
pub struct SkillMatch {
    /// The `skill` tool accepts name or slug; the hint uses the slug — it
    /// is stable, lowercase, and what the registry test enforces.
    pub slug: String,
    pub description: String,
    pub dense: f32,
    pub sparse: f32,
}

struct Entry {
    slug: String,
    description: String,
    enabled: bool,
}

pub struct SkillMatcher {
    entries: Vec<Entry>,
    dense: Vec<Vec<f32>>,
    sparse: SparseIndex,
    embedder: Embedder,
}

impl SkillMatcher {
    /// Load the embedding model (downloading on first run) and embed the
    /// whole skill corpus. Blocking — call from `spawn_blocking`.
    pub fn build(skills: &[SkillDefinition], cache_dir: PathBuf) -> Result<Self, String> {
        let mut embedder = Embedder::init(cache_dir)?;

        let corpus: Vec<String> = skills.iter().map(corpus_text).collect();
        let dense = embedder.embed_passages(&corpus)?;
        let sparse = SparseIndex::build(&corpus);
        let entries = skills
            .iter()
            .map(|s| Entry {
                slug: s.slug.clone(),
                description: s.description.clone(),
                enabled: s.enabled,
            })
            .collect();

        Ok(Self { entries, dense, sparse, embedder })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Rank the corpus against `prompt` and return the top matches.
    ///
    /// Candidates are ordered by RRF over the dense and sparse rankings,
    /// then filtered: skills whose full text is already in the system
    /// prompt (`enabled`) are skipped, and a candidate must clear the dense
    /// floor or have at least some lexical overlap — this is what keeps the
    /// hint quiet on small talk instead of always suggesting *something*.
    pub fn query(&mut self, prompt: &str, top_k: usize, min_dense: f32) -> Vec<SkillMatch> {
        if self.entries.is_empty() || top_k == 0 {
            return Vec::new();
        }
        let query_vec = match self.embedder.embed_query(prompt) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("semantic: query embedding failed: {e}");
                return Vec::new();
            }
        };

        let dense_scores: Vec<f32> = self.dense.iter().map(|d| dot(&query_vec, d)).collect();
        let sparse_scores = self.sparse.scores(prompt);
        let fused = rrf_fuse(&[&dense_scores, &sparse_scores]);

        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by(|&a, &b| fused[b].total_cmp(&fused[a]));

        order
            .into_iter()
            .filter(|&i| !self.entries[i].enabled)
            .filter(|&i| dense_scores[i] >= min_dense || sparse_scores[i] > 1e-6)
            .take(top_k)
            .map(|i| SkillMatch {
                slug: self.entries[i].slug.clone(),
                description: self.entries[i].description.clone(),
                dense: dense_scores[i],
                sparse: sparse_scores[i],
            })
            .collect()
    }

    /// Full ranking by RRF with no filtering — used by the eval harness to
    /// compute MRR over the entire corpus.
    #[cfg(test)]
    pub fn rank_all(&mut self, prompt: &str) -> Vec<String> {
        let n = self.entries.len();
        let before: Vec<SkillMatch> = self.query(prompt, n, f32::MIN);
        before.into_iter().map(|m| m.slug).collect()
    }
}

/// What the embedder and the sparse index see for one skill. Name and slug
/// carry the exact keywords; the description carries the semantics.
fn corpus_text(skill: &SkillDefinition) -> String {
    format!("{} ({}): {}", skill.name, skill.slug, skill.description)
}

/// Reciprocal Rank Fusion over several score lists (all in corpus order).
/// Returns one fused score per document.
fn rrf_fuse(score_lists: &[&Vec<f32>]) -> Vec<f32> {
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
