pub mod deepseek;
#[cfg(test)]
pub mod fake;
pub mod openai_compat;
pub mod prompt;
pub mod pow;
pub mod sse;
pub mod types;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip)]
    pub display_content: Option<String>,
    #[serde(skip)]
    pub tool_error: bool,
    /// UI-only notice (status lines, goal-cycle chrome). Rendered in the chat
    /// but filtered out of what is sent to the provider, so decorating the UI
    /// can never silently change what the model sees.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ui_only: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub total_tokens: Option<u32>,
    #[serde(skip)]
    pub model: String,
    #[serde(skip)]
    pub status: Option<String>,
    #[serde(skip)]
    pub think_elapsed_secs: f64,
    #[serde(skip)]
    pub references_count: u32,
    #[serde(skip)]
    pub search_triggered: bool,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn is_false(value: &bool) -> bool {
    !value
}

impl ChatMessage {
    pub fn system(content: &str) -> Self {
        Self {
            role: Role::System,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            display_content: None,
            tool_error: false,
            ui_only: false,
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: Role::User,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            display_content: None,
            tool_error: false,
            ui_only: false,
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            display_content: None,
            tool_error: false,
            ui_only: false,
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    pub fn tool(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            name: None,
            tool_call_id: Some(tool_call_id.to_string()),
            display_content: None,
            tool_error: false,
            ui_only: false,
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    pub fn tool_with_display(
        tool_call_id: &str,
        tool_name: &str,
        content: &str,
        display: &str,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            name: Some(tool_name.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            display_content: Some(display.to_string()),
            tool_error: is_error,
            ui_only: false,
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    /// A system-styled notice shown in the chat but never sent to the model.
    pub fn ui_system(content: &str) -> Self {
        Self {
            ui_only: true,
            ..Self::system(content)
        }
    }

    /// A real user message whose chat rendering is a short label instead of
    /// the full content (e.g. goal-retry prompts).
    pub fn user_with_display(content: &str, display: &str) -> Self {
        Self {
            display_content: Some(display.to_string()),
            ..Self::user(content)
        }
    }

    pub fn visible_content(&self) -> &str {
        self.display_content.as_deref().unwrap_or(&self.content)
    }
}

/// A server-side chat session as shown in the `/delete` picker.
#[derive(Debug, Clone)]
pub struct RemoteSessionInfo {
    pub id: String,
    pub title: String,
    /// RFC 3339, when the provider reports it.
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttachedFile {
    pub display_name: String,
    pub path: String,
    pub size: u64,
    pub is_image: bool,
}

pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() / 4).max(1) as u32
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    // Populated at every call site (true for streaming turns, false for the
    // ACP and goal-evaluator one-shots) but the provider picks streaming vs.
    // non-streaming by which trait method the caller invokes
    // (`complete`/`complete_stream`), not by reading this back.
    #[expect(dead_code, reason = "streaming choice is made by which trait method is called, not this flag")]
    pub stream: bool,
}

#[derive(Debug, Clone)]
pub struct CompletionChunk {
    pub content: String,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    // Populated by every `complete()` implementation but no caller
    // (src/acp/server.rs, app/goal.rs) reads past `.content`.
    #[expect(dead_code, reason = "no caller of complete() reads finish_reason/usage yet")]
    pub finish_reason: Option<String>,
    #[expect(dead_code, reason = "no caller of complete() reads finish_reason/usage yet")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> crate::error::AppResult<CompletionResponse>;
    async fn complete_stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::UnboundedSender<CompletionChunk>,
    ) -> crate::error::AppResult<()>;
    fn model(&self) -> &str;

    /// Create a sibling instance that shares this provider's configuration
    /// (and connection) but starts a **fresh session** — no shared
    /// `parent_message_id`/`session_id`. Each parallel conversation and
    /// sub-agent gets its own fork so concurrent turns never collide on
    /// session state.
    fn fork(&self) -> std::sync::Arc<dyn LLMProvider>;
    async fn reset(&self) -> crate::error::AppResult<()> {
        Ok(())
    }

    /// Delete this instance's server-side session, if one was created.
    /// Called when an ephemeral conversation (sidechat / sub-agent fork) is
    /// discarded, so one-shot runs don't pile up junk chats on the user's
    /// account. Main chats are never discarded — the user may want to
    /// continue them from the web UI. Default: nothing to clean.
    async fn discard_remote_session(&self) -> crate::error::AppResult<()> {
        Ok(())
    }

    /// List the account's server-side chat sessions (for `/delete`).
    /// Default: unsupported.
    async fn list_remote_sessions(&self) -> crate::error::AppResult<Vec<RemoteSessionInfo>> {
        Err(crate::error::AppError::Custom(
            "Remote session listing not supported by this provider".to_string(),
        ))
    }

    /// Delete a server-side session by its id (for `/delete`).
    /// Default: unsupported.
    async fn delete_remote_session_by_id(
        &self,
        _session_id: &str,
    ) -> crate::error::AppResult<()> {
        Err(crate::error::AppError::Custom(
            "Remote session deletion not supported by this provider".to_string(),
        ))
    }

    async fn fetch_remote_session_messages(
        &self,
        _session_id: &str,
    ) -> crate::error::AppResult<Vec<ChatMessage>> {
        Err(crate::error::AppError::Custom(
            "Remote session fetching not supported by this provider".to_string(),
        ))
    }

    /// This provider's live server-side session identity (session id + last
    /// known parent message id), if a turn has established one since the
    /// last `reset()`. Read synchronously (in-memory only, no network) so
    /// it can be sampled on every auto-save without blocking. `None` before
    /// the first turn of a session, or right after a reset.
    fn session_identity(&self) -> Option<(String, Option<i64>)> {
        None
    }

    /// Best-effort check that a previously-established server-side session
    /// id is still reachable (hasn't been deleted or expired upstream).
    /// `false` covers both "confirmed gone" and "couldn't tell" — callers
    /// must not keep threading onto a session they can't verify.
    async fn session_is_alive(&self, _session_id: &str) -> bool {
        false
    }

    /// Adopt a previously-known server-side session id + parent message id
    /// as this provider's active thread, so the next turn continues it
    /// instead of creating a new remote session. Callers should confirm the
    /// session is still alive (`session_is_alive`) first.
    async fn adopt_session(
        &self,
        _session_id: &str,
        _parent_message_id: Option<i64>,
    ) -> crate::error::AppResult<()> {
        Ok(())
    }
}
