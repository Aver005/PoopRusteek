//! Background persistence worker.
//!
//! Per-turn session saves and per-submit history writes used to run inline
//! in `handle_event` — a full pretty-JSON re-serialization plus an atomic
//! file rewrite whose cost grows with conversation length, freezing input
//! and rendering at the end of every turn (violating the "no file I/O on
//! the event loop" invariant). This worker executes those writes on
//! `spawn_blocking`, one at a time in submission order.
//!
//! Ordering is load-bearing: each job snapshots full state at enqueue time,
//! so the newest snapshot must also be the *last* write to its file.

use crate::provider::ChatMessage;
use crate::semantic::SemanticService;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(crate) enum PersistJob {
    /// Snapshot of everything `session::save_session` needs, taken on the
    /// event loop at enqueue time.
    SaveSession {
        session_id: String,
        created_at: String,
        messages: Vec<ChatMessage>,
        config: Box<crate::config::Config>,
        workspace_path: String,
        meta: crate::session::SessionMeta,
    },
    /// Replace `history.json` with this already-transformed list — the
    /// in-memory recall list is the source of truth (see
    /// `session::push_history_entry`), the file just mirrors it.
    WriteHistory(Vec<String>),
    /// Ack once every previously queued job has completed.
    Flush(oneshot::Sender<()>),
}

/// Handle to the persistence worker task. Enqueueing never blocks; the
/// worker owns the semantic handle so a successful session save can feed
/// the history index exactly like the old inline path did.
pub(crate) struct Persister {
    tx: mpsc::UnboundedSender<PersistJob>,
}

impl Persister {
    pub fn start(semantic: Arc<SemanticService>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<PersistJob>();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                match job {
                    PersistJob::Flush(ack) => {
                        let _ = ack.send(());
                    }
                    job => {
                        let semantic = Arc::clone(&semantic);
                        if let Err(e) =
                            tokio::task::spawn_blocking(move || run_job(job, semantic)).await
                        {
                            tracing::warn!("persist job panicked: {e}");
                        }
                    }
                }
            }
        });
        Self { tx }
    }

    pub fn enqueue(&self, job: PersistJob) {
        // A send error means the worker task is gone (runtime shutdown) —
        // nothing useful left to do with the job.
        let _ = self.tx.send(job);
    }

    /// Wait (bounded) until every job queued so far has been written.
    /// Called before app exit and before `/wipe` — a still-queued save
    /// must not be lost on quit, nor re-create files a wipe just deleted.
    pub async fn flush(&self, timeout: std::time::Duration) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(PersistJob::Flush(ack_tx)).is_ok() {
            let _ = tokio::time::timeout(timeout, ack_rx).await;
        }
    }
}

fn run_job(job: PersistJob, semantic: Arc<SemanticService>) {
    match job {
        PersistJob::SaveSession {
            session_id,
            created_at,
            messages,
            config,
            workspace_path,
            meta,
        } => {
            let result = crate::session::save_session(
                &session_id,
                &created_at,
                &messages,
                &config,
                &workspace_path,
                &meta,
            );
            match result {
                Err(e) => tracing::warn!("Failed to auto-save session: {e}"),
                Ok(()) => {
                    // Session persisted — feed its new messages to the
                    // history index (no-op while semantic matching is
                    // off/initializing; the startup backfill catches up
                    // from the file later).
                    let title = crate::session::derive_title(&messages);
                    semantic.index_session(session_id, title, messages);
                }
            }
        }
        PersistJob::WriteHistory(entries) => crate::session::write_history(&entries),
        PersistJob::Flush(_) => unreachable!("Flush is handled by the worker loop"),
    }
}
