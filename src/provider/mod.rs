pub mod deepseek;
#[cfg(test)]
pub mod fake;
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

impl ChatMessage {
    pub fn system(content: &str) -> Self {
        Self {
            role: Role::System,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            display_content: None,
            tool_error: false,
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
            created_at: now_rfc3339(),
            total_tokens: None,
            model: String::new(),
            status: None,
            think_elapsed_secs: 0.0,
            references_count: 0,
            search_triggered: false,
        }
    }

    pub fn visible_content(&self) -> &str {
        self.display_content.as_deref().unwrap_or(&self.content)
    }
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
    pub finish_reason: Option<String>,
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
    async fn reset(&self) -> crate::error::AppResult<()> {
        Ok(())
    }

    async fn fetch_remote_session_messages(
        &self,
        _session_id: &str,
    ) -> crate::error::AppResult<Vec<ChatMessage>> {
        Err(crate::error::AppError::Custom(
            "Remote session fetching not supported by this provider".to_string(),
        ))
    }
}
