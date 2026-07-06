//! Headless sub-agent runner.
//!
//! A sub-agent runs an isolated agent loop on a **forked provider** (its own
//! session, no parent history) and returns only its final text — the parent
//! never sees the intermediate steps. Tools run without an approval prompt
//! (no user is watching), and `task`/`question` are refused so a sub-agent
//! can't spawn sub-agents or block on a prompt (depth limit of 1).

use crate::agent::stream::{StreamEnd, collect_stream};
use crate::agent::tool_parser::{parse_tool_calls, strip_tool_calls};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::tools::registry::ToolRegistry;
use crate::tools::{QUESTION_TOOL_NAME, TASK_TOOL_NAME};
use std::sync::Arc;

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

        // Headless: no progress callback — nothing streams to a UI.
        let outcome = collect_stream(&provider, request, |_| {}).await;
        // Unlike the main loop, a completed-but-stopless stream is not
        // treated as an error here — the parent only cares about final text.
        match outcome.end {
            StreamEnd::IdleTimeout => return Err("sub-agent stream timed out".to_string()),
            StreamEnd::Completed => {}
            StreamEnd::ProviderError(error) if !outcome.got_stop => return Err(error),
            StreamEnd::ProviderError(_) => {}
            StreamEnd::TaskFailed(join_error) if !outcome.got_stop => {
                return Err(format!("sub-agent stream task failed: {join_error}"));
            }
            StreamEnd::TaskFailed(_) => {}
        }
        let full = outcome.text;

        let tool_calls = parse_tool_calls(&full);
        let visible = strip_tool_calls(&full);

        if tool_calls.is_empty() {
            return Ok(visible);
        }

        messages.push(ChatMessage::assistant(&visible));
        let total_calls = tool_calls.len();
        for (call_index, tool_call) in tool_calls.into_iter().enumerate() {
            let tool_id = uuid::Uuid::new_v4().to_string();
            // Same contract as the main loop: dropped calls get an explicit
            // skipped tool_result instead of silently vanishing.
            if call_index >= max_tools_per_step {
                messages.push(ChatMessage::tool(
                    &tool_id,
                    &format!(
                        "Skipped: per-step tool-call limit of {max_tools_per_step} reached \
                        ({total_calls} calls requested). Re-issue this call next step if still needed."
                    ),
                ));
                continue;
            }
            let result = if tool_call.name == TASK_TOOL_NAME || tool_call.name == QUESTION_TOOL_NAME
            {
                format!("'{}' is not available inside a sub-agent.", tool_call.name)
            } else if tool_call.name.starts_with(crate::mcp::MCP_TOOL_PREFIX) {
                // Same lock discipline as the main loop: never hold the
                // manager mutex across the network await.
                let client = { mcp.lock().await.client_for(&tool_call.name) };
                match client {
                    Some((client, bare_name)) => {
                        match client
                            .call_tool(&bare_name, tool_call.arguments.clone())
                            .await
                        {
                            Ok(r) => r.content,
                            Err(e) => e.to_string(),
                        }
                    }
                    None => format!(
                        "MCP tool '{}' is not available (server not connected)",
                        tool_call.name
                    ),
                }
            } else {
                tools
                    .execute(&tool_call.name, tool_call.arguments.clone())
                    .await
                    .content
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
