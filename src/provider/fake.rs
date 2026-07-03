//! In-memory [`LLMProvider`] test double.
//!
//! This is the second adapter behind the provider seam: with a real
//! `DeepseekProvider` on one side and a `FakeProvider` on the other, the agent
//! loop can be exercised end-to-end without network, wasm proof-of-work, or a
//! DeepSeek token. Two adapters justify the seam.

use super::*;
use crate::error::AppResult;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// A scripted provider. Each `complete`/`complete_stream` call pops the next
/// queued response body; when the queue is empty it yields an empty body.
pub struct FakeProvider {
    model: String,
    responses: Mutex<VecDeque<String>>,
    /// Number of chunks each streamed body is split into (to exercise the
    /// runner's incremental delta handling). Always at least 1.
    chunk_count: usize,
    /// When true, `complete_stream` closes the channel without sending a final
    /// stop chunk, simulating a provider that ends the stream abruptly. The
    /// count is decremented per streamed call, so tests can model "fail once,
    /// recover next call" flows.
    abrupt_eof_remaining: Mutex<usize>,
}

impl FakeProvider {
    /// One scripted response, streamed as a single chunk.
    pub fn with_response(text: impl Into<String>) -> Self {
        Self::with_responses(vec![text.into()])
    }

    /// A queue of scripted responses, consumed front-to-back across calls.
    pub fn with_responses(responses: Vec<String>) -> Self {
        Self {
            model: "fake".to_string(),
            responses: Mutex::new(responses.into_iter().collect()),
            chunk_count: 1,
            abrupt_eof_remaining: Mutex::new(0),
        }
    }

    /// Split each streamed body into `n` chunks instead of one.
    pub fn chunked(mut self, n: usize) -> Self {
        self.chunk_count = n.max(1);
        self
    }

    /// Simulate a streaming provider that drops the channel without sending
    /// the terminal stop chunk.
    pub fn abrupt_eof(mut self) -> Self {
        *self.abrupt_eof_remaining.get_mut().unwrap() = 1;
        self
    }

    fn next_body(&self) -> String {
        self.responses.lock().unwrap().pop_front().unwrap_or_default()
    }
}

#[async_trait]
impl LLMProvider for FakeProvider {
    async fn complete(&self, _request: CompletionRequest) -> AppResult<CompletionResponse> {
        Ok(CompletionResponse {
            content: self.next_body(),
            finish_reason: Some("stop".to_string()),
            usage: None,
        })
    }

    async fn complete_stream(
        &self,
        _request: CompletionRequest,
        tx: UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        let chars: Vec<char> = self.next_body().chars().collect();
        let per = chars.len().div_ceil(self.chunk_count).max(1);
        for piece in chars.chunks(per) {
            let _ = tx.send(CompletionChunk {
                content: piece.iter().collect(),
                finish_reason: None,
            });
        }
        let should_abrupt = {
            let mut remaining = self.abrupt_eof_remaining.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                true
            } else {
                false
            }
        };
        if should_abrupt {
            return Ok(());
        }
        // Terminal chunk carries the stop signal, mirroring the real provider.
        let _ = tx.send(CompletionChunk {
            content: String::new(),
            finish_reason: Some("stop".to_string()),
        });
        Ok(())
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn fork(&self) -> std::sync::Arc<dyn LLMProvider> {
        // A fork starts empty — tests script it independently of the parent.
        std::sync::Arc::new(FakeProvider::with_responses(Vec::new()))
    }
}
