//! Server-side chat session lifecycle: creating a DeepSeek chat session,
//! reusing/resetting it across turns, and tracking the thread's
//! `parent_message_id` so replies stay attached to the right branch.

use super::DeepseekProvider;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use serde_json::{Value, json};

const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";

#[derive(Debug)]
pub(super) struct SessionState {
    pub(super) session_id: Option<String>,
    pub(super) parent_message_id: Option<i64>,
    pub(super) system_sent_for_session: bool,
    /// Budget tokens this server-side session has been fed since it was
    /// minted. `None` = the session came from elsewhere (`adopt_session`), so
    /// what the server still holds is unknown.
    pub(super) session_tokens: Option<u32>,
}

impl Default for SessionState {
    /// No session yet means nothing has been sent yet — zero, not unknown.
    /// Every reset path goes through here, which is what keeps the counter
    /// honest across `reset()`, `discard_remote_session()` and `fork()`.
    fn default() -> Self {
        Self {
            session_id: None,
            parent_message_id: None,
            system_sent_for_session: false,
            session_tokens: Some(0),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SessionSnapshot {
    pub(super) session_id: String,
    pub(super) parent_message_id: Option<i64>,
    pub(super) system_sent_for_session: bool,
}

impl DeepseekProvider {
    pub(super) async fn create_session(&self) -> AppResult<String> {
        let body = json!({ "character_id": Value::Null });
        let headers = self.auth_headers()?;
        let response = self
            .send_json_request(
                "session.create.request",
                CREATE_SESSION_URL,
                &headers,
                &body,
            )
            .await?;

        let status = response.status();
        if !status.is_success() {
            return Err(Self::read_error_response(
                "session.create.request",
                response,
                "Session creation failed",
            )
            .await);
        }

        let payload: Value = response.json().await.map_err(|error| {
            debug_log::log(
                "session.create.parse",
                format!("failed to parse session response json: {error}"),
            );
            AppError::Http(error)
        })?;
        debug_log::log_json("session.create.response", &payload);
        let session_id = payload["data"]["biz_data"]["chat_session"]["id"]
            .as_str()
            .map(|id| id.to_string())
            .ok_or_else(|| {
                AppError::Provider("Invalid session payload: missing chat_session.id".to_string())
            })?;
        debug_log::log("session.create.success", format!("session_id={session_id}"));
        Ok(session_id)
    }

    pub(super) async fn ensure_session(&self, should_reset: bool) -> AppResult<SessionSnapshot> {
        {
            let state = self
                .session_state
                .lock()
                .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;

            if !should_reset && let Some(session_id) = &state.session_id {
                return Ok(SessionSnapshot {
                    session_id: session_id.clone(),
                    parent_message_id: state.parent_message_id,
                    system_sent_for_session: state.system_sent_for_session,
                });
            }
        }

        let session_id = self.create_session().await?;
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;
        state.session_id = Some(session_id.clone());
        state.parent_message_id = None;
        state.system_sent_for_session = false;
        // The server just forgot everything; so does the meter.
        state.session_tokens = Some(0);
        debug_log::log(
            "session.ensure",
            format!("initialized fresh session_id={session_id}, reset={should_reset}"),
        );

        Ok(SessionSnapshot {
            session_id,
            parent_message_id: None,
            system_sent_for_session: false,
        })
    }

    /// Add what was actually put on the wire to the current session's tally.
    /// A session whose size is unknown stays unknown — adding to a guess would
    /// invent a number the caller then trusts.
    pub(super) fn add_session_tokens(&self, tokens: u32) {
        if tokens == 0 {
            return;
        }
        if let Ok(mut state) = self.session_state.lock()
            && let Some(total) = state.session_tokens.as_mut()
        {
            *total = total.saturating_add(tokens);
        }
    }

    /// What this instance's live session has accumulated, in budget tokens.
    pub(super) fn session_tokens_used(&self) -> Option<u32> {
        self.session_state.lock().ok()?.session_tokens
    }

    pub(super) fn mark_session_after_success(
        &self,
        session_id: &str,
        parent_message_id: Option<i64>,
    ) -> AppResult<()> {
        let mut state = self
            .session_state
            .lock()
            .map_err(|_| AppError::Provider("Session state lock poisoned".to_string()))?;

        if state.session_id.as_deref() == Some(session_id) {
            state.system_sent_for_session = true;
            if parent_message_id.is_some() {
                state.parent_message_id = parent_message_id;
            }
        }

        Ok(())
    }
}
