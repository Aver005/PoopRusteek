//! Shared streaming collector for the main agent loop and headless
//! sub-agents. Owns the one tricky invariant both had copy-pasted: the
//! provider stream must run in its **own task** so the idle timeout races
//! the live network — awaiting `complete_stream` inline would buffer the
//! whole response before the first chunk was read (no visible streaming),
//! and a stalled connection (open socket, no bytes) would hang the turn
//! with the timeout never firing.
//!
//! The collector reports *what happened* ([`StreamEnd`]) and leaves the
//! interpretation to the caller: the main loop treats a completed-but-
//! stopless stream as a provider error (with a tool-call salvage path),
//! while a sub-agent deliberately does not.

use crate::provider::{CompletionRequest, LLMProvider};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a stream may stay silent before the turn is cancelled.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How the spawned `complete_stream` task ended.
pub enum StreamEnd {
    /// No chunk arrived within [`STREAM_IDLE_TIMEOUT`]; the stream task was
    /// aborted. `text` holds whatever arrived before the stall.
    IdleTimeout,
    /// The provider task returned `Ok(())`. Note this alone does not mean a
    /// well-formed response — check [`StreamOutcome::got_stop`].
    Completed,
    /// The provider task returned an error (stringified).
    ProviderError(String),
    /// The spawned task itself failed to join (panic or external abort).
    TaskFailed(String),
}

pub struct StreamOutcome {
    /// Everything accumulated from the stream, tool-call syntax included.
    pub text: String,
    /// Whether any chunk carried `finish_reason == "stop"` — the provider's
    /// explicit end-of-turn marker, as opposed to the channel just closing.
    pub got_stop: bool,
    pub end: StreamEnd,
}

/// Run `provider.complete_stream(request)` to completion, accumulating the
/// streamed text. `on_progress` is invoked with the full accumulated text
/// after every non-empty chunk — the main loop derives visible deltas from
/// it; headless callers pass a no-op closure.
pub async fn collect_stream(
    provider: &Arc<dyn LLMProvider>,
    request: CompletionRequest,
    mut on_progress: impl FnMut(&str),
) -> StreamOutcome {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stream_provider = Arc::clone(provider);
    let stream_task = tokio::spawn(async move {
        stream_provider.complete_stream(request, tx).await
    });

    let mut text = String::new();
    let mut got_stop = false;
    loop {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, rx.recv()).await {
            Err(_) => {
                stream_task.abort();
                return StreamOutcome { text, got_stop, end: StreamEnd::IdleTimeout };
            }
            Ok(None) => break,
            Ok(Some(chunk)) => {
                if !chunk.content.is_empty() {
                    text.push_str(&chunk.content);
                    on_progress(&text);
                }
                if matches!(chunk.finish_reason.as_deref(), Some("stop")) {
                    got_stop = true;
                    break;
                }
            }
        }
    }

    // A closed channel without a "stop" chunk usually means the provider
    // bailed mid-stream — the task's own result says how.
    let end = match stream_task.await {
        Ok(Ok(())) => StreamEnd::Completed,
        Ok(Err(error)) => StreamEnd::ProviderError(error.to_string()),
        Err(join_error) => StreamEnd::TaskFailed(join_error.to_string()),
    };
    StreamOutcome { text, got_stop, end }
}
