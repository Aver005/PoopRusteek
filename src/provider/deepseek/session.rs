//! Server-side chat session lifecycle: creating a DeepSeek chat session,
//! reusing/resetting it across turns, and tracking the thread's
//! `parent_message_id` so replies stay attached to the right branch.

use super::DeepseekProvider;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use serde_json::{Value, json};

const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";

#[derive(Debug, Default)]
pub(super) struct SessionState {
    pub(super) session_id: Option<String>,
    pub(super) parent_message_id: Option<i64>,
    pub(super) system_sent_for_session: bool,
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
