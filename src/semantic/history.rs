//! Persistent message-history index — the third semantic corpus.
//!
//! Indexes user/assistant messages of every *saved session* so `/search`
//! (human) and the `history_search` tool (agent) can recall past
//! conversations. The session files under `Config::sessions_dir()` remain
//! the source of truth; this index is a rebuildable cache of chunked
//! message texts + their embeddings, persisted as one JSON file (vectors
//! base64-encoded f32-LE) through `util::atomic_write`. A model change is
//! detected via the header stamp and simply wipes the cache — the next
//! backfill re-embeds everything from the session files.
//!
//! All mutation/search runs on `spawn_blocking` threads under the
//! service's inner lock, like every other corpus.

use super::embedder::{dot, Embedder, EMBEDDING_DIM, MODEL_ID};
use super::index::rrf_fuse;
use super::sparse::SparseIndex;
use crate::provider::{ChatMessage, Role};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const FILE_VERSION: u32 = 1;
/// Chunking: e5's 512-token training context comfortably fits ~1200 chars
/// of mixed ru/en text; the overlap keeps sentences cut at a boundary
/// findable from either side.
const CHUNK_MAX_CHARS: usize = 1200;
const CHUNK_OVERLAP_CHARS: usize = 150;
/// Memory guard: ~1.6 KB per record (384 × f32 + text) → ~80 MB at the
/// cap. Oldest records are dropped first; sessions are re-listable from
/// disk if ever needed.
const MAX_RECORDS: usize = 50_000;

#[derive(Serialize, Deserialize)]
struct PersistedFile {
    version: u32,
    model: String,
    dim: usize,
    /// session id → number of *messages* (not chunks) already consumed.
    watermarks: HashMap<String, usize>,
    records: Vec<PersistedRecord>,
}

#[derive(Serialize, Deserialize)]
struct PersistedRecord {
    session_id: String,
    title: String,
    role: String,
    timestamp: String,
    text: String,
    vector_b64: String,
}

#[derive(Debug, Clone)]
pub struct HistoryMatch {
    pub session_id: String,
    pub title: String,
    pub role: String,
    pub timestamp: String,
    pub text: String,
}

struct Record {
    session_id: String,
    title: String,
    role: String,
    timestamp: String,
    text: String,
}

pub struct HistoryStore {
    path: PathBuf,
    records: Vec<Record>,
    dense: Vec<Vec<f32>>,
    sparse: SparseIndex,
    watermarks: HashMap<String, usize>,
}

impl HistoryStore {
    /// Load the persisted index, or start empty when the file is missing,
    /// unparsable, or was built by a different model (wipe-and-rebuild —
    /// session files are the source of truth).
    pub fn open(path: PathBuf) -> Self {
        let mut store = Self {
            path,
            records: Vec::new(),
            dense: Vec::new(),
            sparse: SparseIndex::build(&[]),
            watermarks: HashMap::new(),
        };

        let Ok(json) = std::fs::read_to_string(&store.path) else {
            return store;
        };
        let parsed: PersistedFile = match serde_json::from_str(&json) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("semantic: history index unreadable, rebuilding: {e}");
                return store;
            }
        };
        if parsed.version != FILE_VERSION || parsed.model != MODEL_ID || parsed.dim != EMBEDDING_DIM
        {
            tracing::info!(
                "semantic: history index built by {}/{} — wiping for {MODEL_ID}/{EMBEDDING_DIM}",
                parsed.model,
                parsed.dim
            );
            return store;
        }

        for r in parsed.records {
            let Some(vector) = decode_vector(&r.vector_b64) else {
                tracing::warn!("semantic: dropping history record with bad vector");
                continue;
            };
            store.dense.push(vector);
            store.records.push(Record {
                session_id: r.session_id,
                title: r.title,
                role: r.role,
                timestamp: r.timestamp,
                text: r.text,
            });
        }
        store.watermarks = parsed.watermarks;
        store.rebuild_sparse();
        store
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn session_count(&self) -> usize {
        self.watermarks.len()
    }

    /// How many messages of `session_id` are already indexed — lets the
    /// backfill skip sessions whose files haven't grown.
    pub fn watermark(&self, session_id: &str) -> usize {
        self.watermarks.get(session_id).copied().unwrap_or(0)
    }

    /// Index everything new in one session. `messages` is the session's
    /// FULL message list — the per-session watermark decides what's new.
    /// A shrunken list (cleared/edited session) drops the session's
    /// records and reindexes from scratch. Returns the number of chunks
    /// added; `Ok(0)` means nothing to do (and nothing was written).
    pub fn index_session(
        &mut self,
        embedder: &mut Embedder,
        session_id: &str,
        title: &str,
        messages: &[ChatMessage],
    ) -> Result<usize, String> {
        let mut watermark = self.watermark(session_id);
        if messages.len() < watermark {
            self.purge_session(session_id);
            watermark = 0;
        }

        let mut texts = Vec::new();
        let mut metas = Vec::new();
        for message in &messages[watermark..] {
            if !matches!(message.role, Role::User | Role::Assistant) || message.ui_only {
                continue;
            }
            let content = message.content.trim();
            if content.is_empty() {
                continue;
            }
            let role = match message.role {
                Role::User => "user",
                _ => "assistant",
            };
            for chunk in chunk_text(content, CHUNK_MAX_CHARS, CHUNK_OVERLAP_CHARS) {
                metas.push((role, message.created_at.clone()));
                texts.push(chunk);
            }
        }

        self.watermarks.insert(session_id.to_string(), messages.len());
        if texts.is_empty() {
            return Ok(0);
        }

        let added = texts.len();
        let vectors = embedder.embed_passages(&texts)?;
        for ((text, (role, timestamp)), vector) in texts.into_iter().zip(metas).zip(vectors) {
            self.records.push(Record {
                session_id: session_id.to_string(),
                title: title.to_string(),
                role: role.to_string(),
                timestamp,
                text,
            });
            self.dense.push(vector);
        }

        self.enforce_cap();
        self.rebuild_sparse();
        self.save();
        Ok(added)
    }

    /// Hybrid search, best-first. Explicit lookup → no dense floor; a
    /// document only needs signal in either channel to rank.
    pub fn search(
        &self,
        embedder: &mut Embedder,
        query: &str,
        limit: usize,
    ) -> Result<Vec<HistoryMatch>, String> {
        if self.records.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let query_vec = embedder.embed_query(query)?;
        let dense_scores: Vec<f32> = self.dense.iter().map(|d| dot(&query_vec, d)).collect();
        let sparse_scores = self.sparse.scores(query);
        let fused = rrf_fuse(&[&dense_scores, &sparse_scores]);

        let mut order: Vec<usize> = (0..self.records.len()).collect();
        order.sort_by(|&a, &b| fused[b].total_cmp(&fused[a]));
        Ok(order
            .into_iter()
            .filter(|&i| fused[i] > 0.0)
            .take(limit)
            .map(|i| HistoryMatch {
                session_id: self.records[i].session_id.clone(),
                title: self.records[i].title.clone(),
                role: self.records[i].role.clone(),
                timestamp: self.records[i].timestamp.clone(),
                text: self.records[i].text.clone(),
            })
            .collect())
    }

    fn purge_session(&mut self, session_id: &str) {
        let mut records = Vec::with_capacity(self.records.len());
        let mut dense = Vec::with_capacity(self.dense.len());
        for (record, vector) in std::mem::take(&mut self.records)
            .into_iter()
            .zip(std::mem::take(&mut self.dense))
        {
            if record.session_id != session_id {
                records.push(record);
                dense.push(vector);
            }
        }
        self.records = records;
        self.dense = dense;
        self.watermarks.remove(session_id);
    }

    /// Drop oldest records above `MAX_RECORDS` — a memory guard, loudly.
    fn enforce_cap(&mut self) {
        if self.records.len() <= MAX_RECORDS {
            return;
        }
        let excess = self.records.len() - MAX_RECORDS;
        tracing::warn!("semantic: history index at cap, dropping {excess} oldest records");
        self.records.drain(..excess);
        self.dense.drain(..excess);
    }

    fn rebuild_sparse(&mut self) {
        let texts: Vec<String> = self.records.iter().map(|r| r.text.clone()).collect();
        self.sparse = SparseIndex::build(&texts);
    }

    fn save(&self) {
        let file = PersistedFile {
            version: FILE_VERSION,
            model: MODEL_ID.to_string(),
            dim: EMBEDDING_DIM,
            watermarks: self.watermarks.clone(),
            records: self
                .records
                .iter()
                .zip(&self.dense)
                .map(|(r, v)| PersistedRecord {
                    session_id: r.session_id.clone(),
                    title: r.title.clone(),
                    role: r.role.clone(),
                    timestamp: r.timestamp.clone(),
                    text: r.text.clone(),
                    vector_b64: encode_vector(v),
                })
                .collect(),
        };
        let json = match serde_json::to_string(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("semantic: history index serialize failed: {e}");
                return;
            }
        };
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("semantic: history index dir create failed: {e}");
            return;
        }
        if let Err(e) = crate::util::atomic_write(&self.path, json.as_bytes()) {
            tracing::warn!("semantic: history index write failed: {e}");
        }
    }
}

fn encode_vector(v: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    BASE64.encode(bytes)
}

fn decode_vector(b64: &str) -> Option<Vec<f32>> {
    let bytes = BASE64.decode(b64).ok()?;
    if bytes.len() != EMBEDDING_DIM * 4 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// Split on char boundaries into windows of at most `max_chars` with
/// `overlap` chars carried between adjacent chunks. Never slices bytes —
/// multibyte text (ru, emoji) is the normal case here.
pub fn chunk_text(text: &str, max_chars: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }
    let step = max_chars.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start += step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("привет", 100, 10), vec!["привет".to_string()]);
    }

    #[test]
    fn long_text_chunks_overlap_and_cover_everything() {
        let text: String = ('а'..='я').cycle().take(2500).collect();
        let chunks = chunk_text(&text, 1000, 100);
        assert!(chunks.len() >= 3, "got {} chunks", chunks.len());
        // Every chunk fits the budget…
        assert!(chunks.iter().all(|c| c.chars().count() <= 1000));
        // …adjacent chunks share the overlap…
        let first_tail: String = chunks[0].chars().skip(900).collect();
        let second_head: String = chunks[1].chars().take(100).collect();
        assert_eq!(first_tail, second_head);
        // …and the last chunk ends exactly where the text does.
        assert!(text.ends_with(chunks.last().unwrap().as_str()));
    }

    #[test]
    fn vector_roundtrip_via_base64() {
        let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 * 0.5 - 3.0).collect();
        assert_eq!(decode_vector(&encode_vector(&v)), Some(v));
    }

    #[test]
    fn decode_rejects_wrong_dimension() {
        assert_eq!(decode_vector(&BASE64.encode([0u8; 12])), None);
    }

    #[test]
    fn open_on_missing_file_is_empty_and_searchable_state() {
        let store = HistoryStore::open(std::env::temp_dir().join("pooprusteek-no-such-index.json"));
        assert_eq!(store.record_count(), 0);
        assert_eq!(store.session_count(), 0);
        assert_eq!(store.watermark("anything"), 0);
    }
}
