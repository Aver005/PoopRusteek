//! DeepSeek web-API provider.
//!
//! Talks to DeepSeek's reverse-engineered web API (cookie/token auth + local
//! SHA-3 proof-of-work) instead of an official LLM API. This module is split
//! by concern:
//! - `mod.rs` (this file): the `DeepseekProvider` type, construction/forking,
//!   and the `LLMProvider` trait impl.
//! - `http`: transport plumbing (headers, redaction/debug logging, retry
//!   backoff, generic JSON/GET request senders, rate limiting).
//! - `session`: server-side chat session lifecycle (create/ensure/mark).
//! - `stream`: request-body building and SSE event parsing helpers.
//! - `endpoints`: the full reverse-engineered REST surface, including the
//!   large `#[allow(dead_code)]` collection of wrappers kept for parity with
//!   the upstream API but not yet driven by this TUI.
mod endpoints;
mod http;
mod session;
mod stream;

use super::*;
use crate::config::ProviderConfig;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::time::Duration;

use session::SessionState;

pub struct DeepseekProvider {
    client: Client,
    token: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    session_state: Mutex<SessionState>,
    rate_limit_ms: u64,
    rate_limit_per_minute: u32,
    max_retries: i32,
    last_request: Mutex<Instant>,
    request_history: Mutex<VecDeque<Instant>>,
}

impl DeepseekProvider {
    pub fn new(
        config: &ProviderConfig,
        rate_limit_ms: u64,
        rate_limit_per_minute: u32,
        max_retries: i32,
    ) -> AppResult<Self> {
        // `read_timeout` (not `timeout`) so a stalled connection errors out
        // while a healthy long-lived SSE stream keeps flowing: it bounds the
        // gap *between* bytes, not the whole request.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .build()?;

        let provider = Self {
            client,
            token: config.token.clone(),
            model: config.model.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            session_state: Mutex::new(SessionState::default()),
            rate_limit_ms,
            rate_limit_per_minute,
            max_retries,
            last_request: Mutex::new(Instant::now()),
            request_history: Mutex::new(VecDeque::new()),
        };

        debug_log::log(
            "provider.new",
            format!(
                "DeepSeek provider created; model={}, temperature={}, max_tokens={}, token_present={}",
                provider.model,
                provider.temperature,
                provider.max_tokens,
                !provider.token.is_empty()
            ),
        );

        Ok(provider)
    }

    /// Concrete fork: shares the reqwest client (connection pool is internally
    /// Arc'd) and all config, but starts from a clean session so the fork
    /// threads its own conversation. Kept concrete (not just the trait method)
    /// so tests can inspect the resulting session state.
    fn fork_session(&self) -> DeepseekProvider {
        DeepseekProvider {
            client: self.client.clone(),
            token: self.token.clone(),
            model: self.model.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            session_state: Mutex::new(SessionState::default()),
            rate_limit_ms: self.rate_limit_ms,
            rate_limit_per_minute: self.rate_limit_per_minute,
            max_retries: self.max_retries,
            last_request: Mutex::new(Instant::now()),
            request_history: Mutex::new(VecDeque::new()),
        }
    }
}

#[async_trait]
impl LLMProvider for DeepseekProvider {
    async fn list_models(&self) -> AppResult<Vec<String>> {
        // The web API has no model-listing endpoint; these are the two
        // model types the completion endpoint accepts (see prompt.rs's
        // `resolve_model_type`).
        Ok(vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()])
    }

    async fn complete(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let (response, session_id) = self.send_request(&request).await?;
        let mut stream = response.bytes_stream();
        let mut sse = super::sse::SseLineBuffer::new();
        let mut content = String::new();
        let mut parent_message_id = None;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    // Persist the thread id before bailing so an interrupted
                    // collection doesn't fork the conversation on the next turn.
                    let _ = self.mark_session_after_success(&session_id, parent_message_id);
                    debug_log::log(
                        "completion.collect.error",
                        format!("stream_read_error={error} parent_message_id={parent_message_id:?}"),
                    );
                    return Err(error.into());
                }
            };

            for line in sse.push_bytes(&chunk) {
                let Some(event) = stream::process_stream_line(&line) else {
                    // Unrecognized lines are logged so protocol changes stay
                    // visible instead of silently dropping server events.
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        debug_log::log("completion.collect.skipped", trimmed);
                    }
                    continue;
                };

                debug_log::log("completion.collect.line", line.trim());
                if event.parent_message_id.is_some() {
                    parent_message_id = event.parent_message_id;
                    debug_log::log(
                        "completion.collect.parent",
                        format!("parent_message_id={:?}", parent_message_id),
                    );
                    // Persist immediately so a cut stream keeps the thread id.
                    let _ = self.mark_session_after_success(&session_id, parent_message_id);
                }
                if let Some(text) = event.text {
                    debug_log::log(
                        "completion.collect.chunk",
                        format!("text_chunk={}", text),
                    );
                    content.push_str(&text);
                }
                if event.finished {
                    debug_log::log("completion.collect.done", "received explicit finish signal");
                    self.mark_session_after_success(&session_id, parent_message_id)?;
                    return Ok(CompletionResponse {
                        content,
                        finish_reason: Some("stop".to_string()),
                        usage: None,
                    });
                }
            }
        }

        self.mark_session_after_success(&session_id, parent_message_id)?;
        // DeepSeek's web endpoint routinely finishes a response by just
        // closing the connection — no `data: [DONE]`, no status event. A
        // cleanly terminated chunked body is a deliberate server-side close,
        // so treat it as a normal stop; a severed connection surfaces as a
        // read error in the loop above, never as clean EOF.
        debug_log::log(
            "completion.collect.eof",
            format!(
                "clean EOF treated as stop. session_id={session_id} parent_message_id={parent_message_id:?} content_len={}",
                content.len()
            ),
        );
        Ok(CompletionResponse {
            content,
            finish_reason: Some("stop".to_string()),
            usage: None,
        })
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
    ) -> AppResult<()> {
        let (response, session_id) = self.send_request(&request).await?;
        let mut stream = response.bytes_stream();
        let mut sse = super::sse::SseLineBuffer::new();
        let mut parent_message_id = None;
        let mut streamed_bytes = 0usize;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    // The server already advanced this session's message tree;
                    // persist the id we have so the next message threads onto it
                    // instead of forking onto an invisible branch.
                    let _ = self.mark_session_after_success(&session_id, parent_message_id);
                    debug_log::log(
                        "completion.stream.error",
                        format!("stream_read_error={error} parent_message_id={parent_message_id:?}"),
                    );
                    return Err(error.into());
                }
            };

            for line in sse.push_bytes(&chunk) {
                let Some(event) = stream::process_stream_line(&line) else {
                    // Unrecognized lines are logged so protocol changes stay
                    // visible instead of silently dropping server events.
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        debug_log::log("completion.stream.skipped", trimmed);
                    }
                    continue;
                };

                debug_log::log("completion.stream.line", line.trim());
                if event.parent_message_id.is_some() {
                    parent_message_id = event.parent_message_id;
                    debug_log::log(
                        "completion.stream.parent",
                        format!("parent_message_id={:?}", parent_message_id),
                    );
                    // Persist immediately — if the stream is cut after this,
                    // the thread id is already saved.
                    let _ = self.mark_session_after_success(&session_id, parent_message_id);
                }

                if let Some(text) = event.text {
                    debug_log::log(
                        "completion.stream.chunk",
                        format!("text_chunk={}", text),
                    );
                    streamed_bytes += text.len();
                    let _ = tx.send(CompletionChunk {
                        content: text,
                        finish_reason: None,
                    });
                }

                if event.finished {
                    debug_log::log("completion.stream.done", "received explicit finish signal");
                    self.mark_session_after_success(&session_id, parent_message_id)?;
                    let _ = tx.send(CompletionChunk {
                        content: String::new(),
                        finish_reason: Some("stop".to_string()),
                    });
                    return Ok(());
                }
            }
        }

        self.mark_session_after_success(&session_id, parent_message_id)?;
        // DeepSeek's web endpoint routinely finishes a response by just
        // closing the connection — no `data: [DONE]`, no status event. A
        // cleanly terminated chunked body is a deliberate server-side close,
        // so treat it as a normal stop; a severed connection surfaces as a
        // read error in the loop above, never as clean EOF.
        debug_log::log(
            "completion.stream.eof",
            format!(
                "clean EOF treated as stop. session_id={session_id} parent_message_id={parent_message_id:?} streamed_bytes={streamed_bytes}"
            ),
        );
        let _ = tx.send(CompletionChunk {
            content: String::new(),
            finish_reason: Some("stop".to_string()),
        });
        Ok(())
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn fork(&self) -> Arc<dyn LLMProvider> {
        Arc::new(self.fork_session())
    }

    async fn reset(&self) -> AppResult<()> {
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
        *state = SessionState::default();
        Ok(())
    }

    async fn discard_remote_session(&self) -> AppResult<()> {
        // Take the id and clear local state first — even if the delete call
        // fails, this instance must not keep threading onto the old session.
        let session_id = {
            let mut state = self
                .session_state
                .lock()
                .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
            let id = state.session_id.take();
            *state = SessionState::default();
            id
        };
        if let Some(id) = session_id {
            debug_log::log(
                "session.discard",
                format!("deleting ephemeral remote session {id}"),
            );
            self.delete_remote_session(&id).await?;
        }
        Ok(())
    }

    async fn fetch_remote_session_messages(
        &self,
        session_id: &str,
    ) -> AppResult<Vec<ChatMessage>> {
        self.fetch_remote_history(session_id).await
    }

    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        let state = self.session_state.lock().ok()?;
        state.session_id.clone().map(|id| (id, state.parent_message_id))
    }

    async fn session_is_alive(&self, session_id: &str) -> bool {
        // `chat/history` is the cheapest existing endpoint that touches a
        // specific session id; a non-2xx (deleted/expired/not-yours session)
        // surfaces as `Err` here. A transient network error also reads as
        // "not alive" — callers treat "couldn't verify" the same as "gone"
        // rather than risk threading onto a session that silently vanished.
        self.fetch_remote_history(session_id).await.is_ok()
    }

    async fn adopt_session(&self, session_id: &str, parent_message_id: Option<i64>) -> AppResult<()> {
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
        state.session_id = Some(session_id.to_string());
        state.parent_message_id = parent_message_id;
        // The remote thread already has the system prompt/history from
        // whatever session created it — resending it would duplicate context.
        state.system_sent_for_session = true;
        debug_log::log(
            "session.adopt",
            format!("resumed session_id={session_id} parent_message_id={parent_message_id:?}"),
        );
        Ok(())
    }

    async fn list_remote_sessions(&self) -> AppResult<Vec<RemoteSessionInfo>> {
        let sessions = self.fetch_remote_sessions(None).await?;
        Ok(sessions
            .into_iter()
            .map(|s| {
                // The API reports epoch time; guard against seconds vs millis.
                let secs = if s.updated_at > 1_000_000_000_000 {
                    s.updated_at / 1000
                } else {
                    s.updated_at
                };
                RemoteSessionInfo {
                    id: s.id,
                    title: s.title.unwrap_or_else(|| "(untitled)".to_string()),
                    updated_at: chrono::DateTime::from_timestamp(secs, 0)
                        .map(|dt| dt.to_rfc3339()),
                }
            })
            .collect())
    }

    async fn delete_remote_session_by_id(&self, session_id: &str) -> AppResult<()> {
        self.delete_remote_session(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderConfig, ProviderKind};

    fn provider() -> DeepseekProvider {
        // Builds a reqwest client only — no network, no token needed.
        let config = ProviderConfig {
            kind: ProviderKind::Deepseek,
            token: String::new(),
            model: "deepseek-chat".to_string(),
            base_url: None,
            temperature: 0.0,
            max_tokens: 128,
        };
        DeepseekProvider::new(&config, 0, 0, 0).expect("client builds")
    }

    #[test]
    fn parent_id_persists_once_recorded_and_survives_empty_marks() {
        let p = provider();
        p.session_state.lock().unwrap().session_id = Some("sess-1".to_string());

        // A message id seen mid-stream is recorded immediately.
        p.mark_session_after_success("sess-1", Some(42)).unwrap();
        {
            let s = p.session_state.lock().unwrap();
            assert_eq!(s.parent_message_id, Some(42));
            assert!(s.system_sent_for_session);
        }

        // A later mark with no new id (clean end, or interrupted stream) must
        // NOT clobber the recorded thread id — this is exactly what keeps an
        // abnormally-ended turn from forking the conversation onto a branch the
        // web UI never shows.
        p.mark_session_after_success("sess-1", None).unwrap();
        assert_eq!(p.session_state.lock().unwrap().parent_message_id, Some(42));
    }

    #[test]
    fn fork_has_independent_session_state() {
        let parent = provider();
        parent.session_state.lock().unwrap().session_id = Some("parent-sess".to_string());
        parent.mark_session_after_success("parent-sess", Some(42)).unwrap();

        // The fork must NOT inherit the parent's session/thread id — otherwise
        // two parallel conversations would corrupt each other's message tree.
        let forked = parent.fork_session();
        {
            let s = forked.session_state.lock().unwrap();
            assert_eq!(s.session_id, None);
            assert_eq!(s.parent_message_id, None);
            assert!(!s.system_sent_for_session);
        }
        // And mutating the fork must not touch the parent.
        forked.session_state.lock().unwrap().session_id = Some("fork-sess".to_string());
        assert_eq!(
            parent.session_state.lock().unwrap().session_id.as_deref(),
            Some("parent-sess")
        );
    }

    #[test]
    fn mark_ignores_a_stale_session_id() {
        let p = provider();
        {
            let mut s = p.session_state.lock().unwrap();
            s.session_id = Some("current".to_string());
            s.parent_message_id = Some(7);
        }
        // A late event referring to a previous/reset session must not rewrite
        // the current session's thread id.
        p.mark_session_after_success("old-session", Some(999)).unwrap();
        assert_eq!(p.session_state.lock().unwrap().parent_message_id, Some(7));
    }

    #[test]
    fn session_identity_none_before_any_turn() {
        let p = provider();
        assert_eq!(p.session_identity(), None);
    }

    #[test]
    fn session_identity_reflects_live_session_state() {
        let p = provider();
        p.session_state.lock().unwrap().session_id = Some("sess-1".to_string());
        p.mark_session_after_success("sess-1", Some(42)).unwrap();
        assert_eq!(p.session_identity(), Some(("sess-1".to_string(), Some(42))));
    }

    #[tokio::test]
    async fn adopt_session_sets_identity_and_skips_resending_history() {
        let p = provider();
        p.adopt_session("resumed-sess", Some(17)).await.unwrap();
        let s = p.session_state.lock().unwrap();
        assert_eq!(s.session_id.as_deref(), Some("resumed-sess"));
        assert_eq!(s.parent_message_id, Some(17));
        // Must be true — otherwise the next turn would resend the system
        // prompt + local history into a thread that already has them.
        assert!(s.system_sent_for_session);
    }
}
