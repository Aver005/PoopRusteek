//! Stemmed sparse vectors — the lexical half of the hybrid skill match.
//!
//! TF-IDF over Snowball stems: tokens are lowercased alphanumeric runs,
//! Cyrillic tokens go through the Russian stemmer, everything else through
//! the English one. Document and query vectors are L2-normalized so the
//! sparse score is a cosine in [0, 1]; a score of exactly 0 means "no
//! lexical overlap at all", which the matcher uses as a hard signal.

use rust_stemmers::{Algorithm, Stemmer};
use std::collections::{HashMap, HashSet};

/// Lowercase, split into alphanumeric runs, drop 1-char tokens, stem.
pub fn tokenize_stems(text: &str) -> Vec<String> {
    let russian = Stemmer::create(Algorithm::Russian);
    let english = Stemmer::create(Algorithm::English);

    let lowered = text.to_lowercase();
    let mut stems = Vec::new();
    for token in lowered.split(|c: char| !c.is_alphanumeric()) {
        if token.chars().count() < 2 {
            continue;
        }
        let is_cyrillic = token.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
        let stemmer = if is_cyrillic { &russian } else { &english };
        stems.push(stemmer.stem(token).into_owned());
    }
    stems
}

/// A corpus of sparse TF-IDF document vectors with the IDF table used to
/// weight queries the same way.
pub struct SparseIndex {
    docs: Vec<HashMap<String, f32>>,
    idf: HashMap<String, f32>,
}

impl SparseIndex {
    pub fn build(texts: &[String]) -> Self {
        let tokenized: Vec<Vec<String>> = texts.iter().map(|t| tokenize_stems(t)).collect();

        let n = texts.len() as f32;
        let mut df: HashMap<String, f32> = HashMap::new();
        for tokens in &tokenized {
            let unique: HashSet<&String> = tokens.iter().collect();
            for term in unique {
                *df.entry(term.clone()).or_default() += 1.0;
            }
        }
        // BM25-style IDF, shifted to stay positive even for terms present in
        // every document (df == n).
        let idf: HashMap<String, f32> = df
            .into_iter()
            .map(|(term, d)| (term, ((n - d + 0.5) / (d + 0.5) + 1.0).ln()))
            .collect();

        let docs = tokenized.iter().map(|tokens| weighted(tokens, &idf)).collect();
        Self { docs, idf }
    }

    /// Cosine similarity of `query` against every document, in corpus order.
    /// Query terms absent from the corpus carry no signal and are dropped.
    pub fn scores(&self, query: &str) -> Vec<f32> {
        let tokens: Vec<String> = tokenize_stems(query)
            .into_iter()
            .filter(|t| self.idf.contains_key(t))
            .collect();
        if tokens.is_empty() {
            return vec![0.0; self.docs.len()];
        }
        let q = weighted(&tokens, &self.idf);
        self.docs.iter().map(|d| sparse_dot(&q, d)).collect()
    }
}

/// TF × IDF weights for one token list, L2-normalized.
fn weighted(tokens: &[String], idf: &HashMap<String, f32>) -> HashMap<String, f32> {
    let mut tf: HashMap<String, f32> = HashMap::new();
    for token in tokens {
        *tf.entry(token.clone()).or_default() += 1.0;
    }
    let mut weights: HashMap<String, f32> = tf
        .into_iter()
        .map(|(term, count)| {
            let w = count * idf.get(&term).copied().unwrap_or(1.0);
            (term, w)
        })
        .collect();
    let norm = weights.values().map(|w| w * w).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for w in weights.values_mut() {
            *w /= norm;
        }
    }
    weights
}

fn sparse_dot(a: &HashMap<String, f32>, b: &HashMap<String, f32>) -> f32 {
    // Iterate the smaller map for fewer lookups.
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    small
        .iter()
        .filter_map(|(term, wa)| large.get(term).map(|wb| wa * wb))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_word_forms_share_a_stem() {
        // Case/number forms of one noun. (Verb↔noun derivations like
        // тестировать/тестирование do NOT share a Snowball stem — don't
        // "fix" this test by expecting that.)
        let a = tokenize_stems("уязвимостями");
        let b = tokenize_stems("уязвимость");
        assert_eq!(a, b, "case forms of the same Russian word must stem equally");
    }

    #[test]
    fn english_word_forms_share_a_stem() {
        let a = tokenize_stems("testing");
        let b = tokenize_stems("tests");
        assert_eq!(a, b, "different forms of the same English word must stem equally");
    }

    #[test]
    fn tokenizer_splits_mixed_scripts_and_drops_short_tokens() {
        let stems = tokenize_stems("Rust-код: 1 тест, CI!");
        // "1" is a single char and dropped; the rest survive as stems.
        assert!(stems.len() == 4, "expected 4 stems, got {stems:?}");
    }

    #[test]
    fn keyword_query_ranks_the_matching_document_first() {
        let corpus = vec![
            "Telegram bot builder: create bots for Telegram".to_string(),
            "Tailwind CSS styling and layouts".to_string(),
            "Playwright browser automation and e2e tests".to_string(),
        ];
        let index = SparseIndex::build(&corpus);
        let scores = index.scores("напиши telegram бота");
        let best = scores
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i);
        assert_eq!(best, Some(0), "scores: {scores:?}");
        assert!(scores[0] > 0.0);
    }

    #[test]
    fn query_with_no_overlap_scores_zero_everywhere() {
        let corpus = vec!["Tailwind CSS styling".to_string()];
        let index = SparseIndex::build(&corpus);
        let scores = index.scores("квантовая хромодинамика");
        assert_eq!(scores, vec![0.0]);
    }
}
