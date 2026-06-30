//! Headless sub-agent runner.
//!
//! A sub-agent runs an isolated agent loop on a **forked provider** (its own
//! session, no parent history) and returns only its final text — the parent
//! never sees the intermediate steps. Tools run without an approval prompt
//! (no user is watching), and `task`/`question` are refused so a sub-agent
//! can't spawn sub-agents or block on a prompt (depth limit of 1).

use crate::agent::tool_parser::{parse_tool_calls, strip_tool_calls};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::tools::registry::ToolRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;

#[allow(clippy::too_many_arguments)]
pub async fn run_sub_agent(
    provider: Arc<dyn LLMProvider>,
    tools: Arc<ToolRegistry>,
    mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    system_prompt: String,
    user_prompt: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    max_steps: usize,
    max_tools_per_step: usize,
) -> Result<String, String> {
    let mut messages = vec![ChatMessage::user(&user_prompt)];

    for _step in 0..max_steps {
        let mut request_messages = Vec::with_capacity(messages.len() + 1);
        request_messages.push(ChatMessage::system(&system_prompt));
        request_messages.extend(messages.clone());

        let request = CompletionRequest {
            messages: request_messages,
            model: model.clone(),
            temperature,
            max_tokens,
            stream: true,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        provider
            .complete_stream(request, tx)
            .await
            .map_err(|e| e.to_string())?;

        let mut full = String::new();
        let idle = std::time::Duration::from_secs(120);
        loop {
            match tokio::time::timeout(idle, rx.recv()).await {
                Err(_) => return Err("sub-agent stream timed out".to_string()),
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    full.push_str(&chunk.content);
                    if matches!(chunk.finish_reason.as_deref(), Some("stop")) {
                        break;
                    }
                }
            }
        }

        let tool_calls = parse_tool_calls(&full);
        let visible = strip_tool_calls(&full);

        if tool_calls.is_empty() {
            return Ok(visible);
        }

        messages.push(ChatMessage::assistant(&visible));
        for tool_call in tool_calls.into_iter().take(max_tools_per_step) {
            let tool_id = uuid::Uuid::new_v4().to_string();
            let result = if tool_call.name == "task" || tool_call.name == "question" {
                format!("'{}' is not available inside a sub-agent.", tool_call.name)
            } else if tool_call.name.starts_with("mcp__") {
                let mut mcp = mcp.lock().await;
                match mcp.call_tool(&tool_call.name, tool_call.arguments.clone()).await {
                    Ok(r) => r.content,
                    Err(e) => e.to_string(),
                }
            } else {
                tools.execute(&tool_call.name, tool_call.arguments.clone()).await.content
            };
            messages.push(ChatMessage::tool(&tool_id, &result));
        }
    }

    // Hit the step cap — return the best (last assistant) text we have.
    Ok(messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    /// A sub-agent runs headless on its own provider and returns just the final
    /// text — no events, no network.
    #[tokio::test]
    async fn returns_final_text() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response("Done: 42"));
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));

        let out = run_sub_agent(
            provider,
            tools,
            mcp,
            "system".to_string(),
            "compute the answer".to_string(),
            "fake".to_string(),
            0.0,
            128,
            4,
            4,
        )
        .await;

        assert_eq!(out.unwrap(), "Done: 42");
    }
}

