//! Session lifecycle controller: `/load` (local + remote fetch + remote-link
//! aliveness), broken-session recovery, the `/delete` flow, and per-turn
//! auto-save. All network work is spawned off the event loop and reports
//! back through `AppEvent`s (`SessionFetched`, `SessionAvailabilityChecked`,
//! `RemoteSessionsListed`, `SessionsDeleted`).

use super::events::{AppEvent, Modal};
use super::{App, conversation, events};
use crate::error::AppResult;
use crate::provider::ChatMessage;
use crate::session;
use std::sync::Arc;

impl App {
    pub(super) async fn handle_load_session(&mut self, session_id: &str) -> AppResult<()> {
        match session::load_local(session_id, &self.config) {
            Ok(s) => {
                self.state.focused_mut().messages = s.messages.clone();
                self.state.attached_files.clear();
                self.state.focused_mut().session_id = s.id.clone();
                self.state.focused_mut().tag = s.tag.clone();
                self.state.focused_mut().broken = s.broken;
                self.state.scroll_offset = 0;
                self.state.input.buffer.clear();
                self.state.input.cursor = 0;
                self.state.input.selection_anchor = None;
                self.state.autocomplete = Default::default();
                self.state.focused_mut().generation.active = false;
                if let Some(handle) = self.state.focused_mut().agent_task.take() {
                    handle.abort();
                }

                let count = s.messages.len();
                self.state.status_message =
                    format!("Loaded local session {session_id} ({count} messages)");

                if s.broken {
                    self.state.focused_mut().messages.push(ChatMessage::system(
                        "This session's remote DeepSeek link was already found unreachable — \
                        a fresh remote session will be created and your full local history \
                        sent as context with your next message.",
                    ));
                }

                let provider = self.state.focused().provider.clone();
                let conversation = self.state.conversations.focused_id();

                match (provider, s.provider_session_id.clone()) {
                    (Some(provider), Some(remote_id)) => {
                        // Verify off the event loop: a slow/hung network check
                        // here must not freeze the whole TUI. `s` (messages +
                        // metadata) rides along so the result handler can
                        // persist a "confirmed broken" session without
                        // re-reading the file.
                        let parent_message_id = s.provider_parent_message_id;
                        let event_tx = self.event_tx.clone();
                        tokio::spawn(async move {
                            let alive = provider.session_is_alive(&remote_id).await;
                            let _ = event_tx.send(AppEvent::SessionAvailabilityChecked {
                                conversation,
                                session: s,
                                remote_id,
                                parent_message_id,
                                alive,
                            });
                        });
                    }
                    (Some(provider), None) => {
                        // No remote link to verify (never established, or
                        // already cleared by an earlier broken-session
                        // recovery) — just ensure a clean slate so the next
                        // message starts a fresh remote session with local
                        // history replayed as one prompt (default behavior).
                        let _ = provider.reset().await;
                    }
                    (None, _) => {}
                }
            }
            Err(_) => {
                let remote_provider = self.state.focused().provider.clone();
                if let Some(provider) = remote_provider {
                    // Fetch off the event loop — a slow network here used to
                    // freeze the whole TUI until the response (or timeout).
                    let conversation = self.state.conversations.focused_id();
                    let sid = session_id.to_string();
                    let event_tx = self.event_tx.clone();
                    self.state.status_message = format!("Fetching remote session {session_id}...");
                    tokio::spawn(async move {
                        let result = provider
                            .fetch_remote_session_messages(&sid)
                            .await
                            .map_err(|e| e.to_string());
                        let _ = event_tx.send(AppEvent::SessionFetched {
                            conversation,
                            session_id: sid,
                            result,
                        });
                    });
                } else {
                    self.state
                        .focused_mut()
                        .messages
                        .push(ChatMessage::system(&format!(
                            "Session {session_id} not found locally"
                        )));
                }
            }
        }
        Ok(())
    }

    /// Apply the result of a background aliveness check (started by
    /// `handle_load_session`) of a local session's linked remote DeepSeek
    /// session.
    pub(super) async fn apply_session_availability(
        &mut self,
        conversation: conversation::ConversationId,
        session: session::Session,
        remote_id: String,
        parent_message_id: Option<i64>,
        alive: bool,
    ) {
        let Some(conv) = self.state.conversations.get(conversation) else {
            return;
        };
        // The user loaded a different session (or this one again) while the
        // check was in flight — the result no longer applies.
        if conv.session_id != session.id {
            return;
        }
        let provider = conv.provider.clone();

        if alive {
            if let Some(provider) = provider {
                let _ = provider.adopt_session(&remote_id, parent_message_id).await;
            }
            return;
        }

        if let Some(provider) = provider {
            let _ = provider.reset().await;
        }
        self.finalize_broken_session(conversation, session);
    }

    /// Flag a session's remote link as confirmed dead: persist `broken` +
    /// a cleared provider identity to disk, mirror it on the live
    /// `Conversation`, and notify the user. Shared by the "just discovered
    /// broken" path (`apply_session_availability`) — there's no "already
    /// known broken" path calling this because `handle_load_session` skips
    /// the network check entirely once a session has no remote id left to
    /// verify.
    fn finalize_broken_session(
        &mut self,
        conversation: conversation::ConversationId,
        mut session: session::Session,
    ) {
        session.broken = true;
        session.provider_session_id = None;
        session.provider_parent_message_id = None;
        if let Err(e) = session::save_local(&session, &self.config) {
            tracing::warn!("Failed to persist broken-session flag: {e}");
        }

        let Some(conv) = self.state.conversations.get_mut(conversation) else {
            return;
        };
        if conv.session_id != session.id {
            return;
        }
        conv.broken = true;
        conv.messages.push(ChatMessage::system(
            "This session's remote DeepSeek link is no longer reachable — a fresh remote \
            session will be created and your full local history sent as context with your \
            next message.",
        ));
    }

    /// Apply a remote-session fetch result delivered by `SessionFetched`.
    pub(super) async fn apply_fetched_session(
        &mut self,
        conversation: conversation::ConversationId,
        session_id: &str,
        result: Result<Vec<ChatMessage>, String>,
    ) {
        let messages = match result {
            Ok(m) if m.is_empty() => {
                if let Some(conv) = self.state.conversations.get_mut(conversation) {
                    conv.messages.push(ChatMessage::system(&format!(
                        "Remote session {session_id} has no messages"
                    )));
                }
                return;
            }
            Ok(m) => m,
            Err(e) => {
                if let Some(conv) = self.state.conversations.get_mut(conversation) {
                    conv.messages.push(ChatMessage::system(&format!(
                        "Session {session_id} not found locally and remote fetch failed: {e}"
                    )));
                }
                return;
            }
        };

        let provider = self
            .state
            .conversations
            .get(conversation)
            .and_then(|c| c.provider.clone());
        let Some(conv) = self.state.conversations.get_mut(conversation) else {
            return;
        };
        conv.messages = messages;
        conv.session_id = session_id.to_string();
        // A freshly imported remote session has no known-broken history and
        // (see the save below) no locally-confirmed provider identity yet.
        conv.tag = None;
        conv.broken = false;
        conv.generation.active = false;
        if let Some(handle) = conv.agent_task.take() {
            handle.abort();
        }
        let count = conv.messages.len();
        let title = session::derive_title(&conv.messages);
        let snapshot = conv.messages.clone();

        if conversation == self.state.conversations.focused_id() {
            self.state.scroll_offset = 0;
            self.state.input.buffer.clear();
            self.state.input.cursor = 0;
            self.state.input.selection_anchor = None;
            self.state.autocomplete = Default::default();
        }
        if let Some(provider) = provider {
            // Local session-state reset only — no network.
            let _ = provider.reset().await;
        }

        let now = session::timestamp_now();
        if let Err(e) = session::save_local(
            &session::Session {
                version: session::SESSION_VERSION,
                id: session_id.to_string(),
                created_at: now.clone(),
                updated_at: now,
                workspace_root: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                model_type: self.config.provider.model.clone(),
                messages: snapshot,
                tag: None,
                // Not wired to resume yet: `fetch_remote_history` (the only
                // existing endpoint for this path) doesn't parse a message id
                // out of `chat/history`'s items, so there's no reliable
                // `parent_message_id` to adopt. Falls back to the default
                // fresh-session behavior (full local history replayed as one
                // prompt on the next message) — same as any other session
                // with no known provider identity.
                provider_session_id: None,
                provider_parent_message_id: None,
                broken: false,
            },
            &self.config,
        ) {
            tracing::warn!("Failed to save imported session: {e}");
        }
        self.state.status_message =
            format!("Imported remote session {session_id}: {title} ({count} msgs)");
    }

    /// Open the `/delete` picker. Local sessions seed the list synchronously
    /// (fast disk scan); the remote list arrives via `RemoteSessionsListed`
    /// from a spawned fetch so the event loop never waits on the network.
    pub(super) fn open_delete_sessions(
        &mut self,
        scope: events::SessionScope,
        session_id: Option<String>,
    ) {
        use events::{DeleteEntry, DeleteSessionsState, DeleteStage, RemoteListStatus};

        let provider = self.state.focused().provider.clone();

        // Direct-id form (`/delete <id>`): jump straight to confirmation.
        if let Some(id) = session_id {
            let entry = DeleteEntry {
                id: id.clone(),
                title: id.clone(),
                local: crate::session::local_exists(&id),
                // Unknown without a fetch — assume deletable remotely when a
                // provider exists; a "not found" upstream is reported softly.
                remote: provider.is_some(),
                updated_at: String::new(),
            };
            let mut st = DeleteSessionsState::new(
                vec![entry],
                scope,
                if provider.is_some() {
                    RemoteListStatus::Ready
                } else {
                    RemoteListStatus::NoProvider
                },
            );
            st.confirm_ids = vec![id];
            st.stage = DeleteStage::Confirming;
            self.state.modal = Some(Modal::DeleteSessions(st));
            return;
        }

        let entries: Vec<DeleteEntry> = crate::session::list_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|s| DeleteEntry {
                id: s.id,
                title: s.title,
                local: true,
                remote: false,
                updated_at: s.updated_at,
            })
            .collect();

        let remote_status = match &provider {
            Some(provider) => {
                let provider = Arc::clone(provider);
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let result = provider
                        .list_remote_sessions()
                        .await
                        .map_err(|e| e.to_string());
                    let _ = event_tx.send(AppEvent::RemoteSessionsListed { result });
                });
                RemoteListStatus::Loading
            }
            None => RemoteListStatus::NoProvider,
        };

        self.state.modal = Some(Modal::DeleteSessions(DeleteSessionsState::new(
            entries,
            scope,
            remote_status,
        )));
    }

    /// Perform a confirmed `/delete`. Local files are removed synchronously
    /// (fast fs ops); remote deletions run in a spawned batch that reports
    /// back via `SessionsDeleted`. The scope decides WHERE ids are deleted:
    /// `All` = both copies, `Local` = disk only, `Remote` = account only.
    pub(super) async fn execute_delete_sessions(
        &mut self,
        ids: Vec<String>,
        scope: events::SessionScope,
        entries: Vec<events::DeleteEntry>,
    ) {
        use events::SessionScope as Scope;

        let mut deleted = 0usize;
        let mut failed: Vec<String> = Vec::new();
        let mut remote_ids: Vec<String> = Vec::new();
        let mut touched_focused = false;

        for id in &ids {
            let entry = entries.iter().find(|e| &e.id == id);
            let (has_local, has_remote) =
                entry.map(|e| (e.local, e.remote)).unwrap_or((true, true));

            if matches!(scope, Scope::All | Scope::Local) && has_local {
                match crate::session::delete_local(id, &self.config) {
                    Ok(true) => deleted += 1,
                    Ok(false) => {}
                    Err(e) => failed.push(format!("{id} (local: {e})")),
                }
            }
            if matches!(scope, Scope::All | Scope::Remote) && has_remote {
                remote_ids.push(id.clone());
            }
            if self.state.focused().session_id == *id {
                touched_focused = true;
            }
        }

        // The focused chat's session was among the targets — reset the
        // provider's threading so the next message starts a clean session
        // instead of posting onto a deleted remote thread.
        if touched_focused && let Some(provider) = self.state.focused().provider.clone() {
            let _ = provider.reset().await;
        }

        if remote_ids.is_empty() {
            let _ = self
                .event_tx
                .send(AppEvent::SessionsDeleted { deleted, failed });
            return;
        }

        let provider = self.state.focused().provider.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let mut deleted = deleted;
            let mut failed = failed;
            match provider {
                Some(provider) => {
                    for id in remote_ids {
                        match provider.delete_remote_session_by_id(&id).await {
                            Ok(()) => deleted += 1,
                            Err(e) => failed.push(format!("{id} (remote: {e})")),
                        }
                    }
                }
                None => failed.push("remote deletion unavailable: no provider".to_string()),
            }
            let _ = event_tx.send(AppEvent::SessionsDeleted { deleted, failed });
        });
    }

    pub(super) fn auto_save_session(&mut self) {
        let conv = self.state.focused();
        if conv.messages.is_empty() {
            return;
        }

        // A provider that currently reports an identity just completed a
        // turn successfully — that's live proof the remote link works again,
        // so a previously-broken conversation is un-flagged here rather than
        // staying permanently yellow after it's actually recovered.
        let identity = conv.provider.as_ref().and_then(|p| p.session_identity());
        let broken = if identity.is_some() {
            false
        } else {
            conv.broken
        };
        let meta = session::SessionMeta {
            tag: conv.tag.clone(),
            broken,
            provider_session_id: identity.as_ref().map(|(id, _)| id.clone()),
            provider_parent_message_id: identity.and_then(|(_, pm)| pm),
        };

        // Snapshot everything now and hand the write (plus the follow-up
        // semantic indexing on success) to the persist worker — the full
        // pretty-JSON rewrite grows with conversation length and used to
        // block the event loop at the end of every turn. FIFO ordering on
        // the worker guarantees a later turn's save lands after this one.
        self.persister
            .enqueue(super::persist::PersistJob::SaveSession {
                session_id: conv.session_id.clone(),
                created_at: session::timestamp_now(),
                messages: conv.messages.clone(),
                config: Box::new(self.config.clone()),
                workspace_path: self.state.workspace_path.clone(),
                meta,
            });
        self.state.focused_mut().broken = broken;
    }
}

impl App {
    /// `AppEvent::SessionsDeleted` — summarize the `/delete` outcome:
    /// failures are enumerated in the chat line, the status line stays short.
    pub(crate) fn on_sessions_deleted(&mut self, deleted: usize, failed: Vec<String>) {
        let mut message = format!(
            "🗑 Deleted {deleted} session cop{}",
            if deleted == 1 { "y" } else { "ies" }
        );
        if !failed.is_empty() {
            message.push_str(&format!(
                "; {} failed:\n  {}",
                failed.len(),
                failed.join("\n  ")
            ));
        }
        self.state.status_message = if failed.is_empty() {
            "Sessions deleted".to_string()
        } else {
            "Some session deletions failed".to_string()
        };
        self.state
            .focused_mut()
            .messages
            .push(ChatMessage::ui_system(&message));
    }
}
