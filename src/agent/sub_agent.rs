//! Headless sub-agent runner.
//!
//! A sub-agent runs an isolated agent loop on a **forked provider** (its own
//! session, no parent history) and returns only its final text — the parent
//! never sees the intermediate steps. Tools run without an approval prompt
//! (no user is watching), and `task`/`question` are refused so a sub-agent
//! can't spawn sub-agents or block on a prompt (depth limit of 1).

use crate::agent::runner::{
    MAX_MALFORMED_TOOL_RETRIES, build_step_request, dispatch_generic_tool, malformed_tool_feedback,
    tool_skip_message,
};
use crate::agent::stream::{StreamEnd, collect_stream};
use crate::agent::tool_parser::{parse_tool_calls_with_errors, strip_tool_calls};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider, Role};
use crate::tools::registry::ToolRegistry;
use crate::tools::{QUESTION_TOOL_NAME, TASK_TOOL_NAME};
use std::sync::Arc;

#[expect(clippy::too_many_arguments)]
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
    // Inherited from the spawning turn: a sub-agent's history grows the same
    // way, so it gets the same rung-0 cap.
    tool_output_limit: usize,
) -> Result<String, String> {
    let mut messages = vec![ChatMessage::user(&user_prompt)];
    let mut malformed_retries: u32 = 0;

    for _step in 0..max_steps {
        let request =
            build_step_request(&system_prompt, &messages, &model, temperature, max_tokens);

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

        let (tool_calls, parse_errors) = parse_tool_calls_with_errors(&full);
        let visible = strip_tool_calls(&full);

        if tool_calls.is_empty() {
            // Same recovery as the main loop: zero parsed calls plus parse
            // errors means a malformed `<tool_use>`, not a final answer —
            // returning `visible` here silently swallowed the failure (the
            // text before a broken block is usually empty).
            if !parse_errors.is_empty() && malformed_retries < MAX_MALFORMED_TOOL_RETRIES {
                malformed_retries += 1;
                messages.push(ChatMessage::assistant(&full));
                messages.push(ChatMessage::user(&malformed_tool_feedback(&parse_errors)));
                continue;
            }
            if !parse_errors.is_empty() {
                return Err(format!(
                    "sub-agent kept emitting malformed tool calls: {}",
                    parse_errors.join(" | ")
                ));
            }
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
                    &tool_skip_message(max_tools_per_step, total_calls),
                ));
                continue;
            }
            let result = if tool_call.name == TASK_TOOL_NAME || tool_call.name == QUESTION_TOOL_NAME
            {
                format!("'{}' is not available inside a sub-agent.", tool_call.name)
            } else {
                // Shared with the main loop (same short-lock MCP discipline).
                // A sub-agent reports errors as plain result text, so the
                // error flag is dropped.
                dispatch_generic_tool(&tools, &mcp, &tool_call.name, tool_call.arguments.clone())
                    .await
                    .0
            };
            messages.push(ChatMessage::tool(
                &tool_id,
                &crate::context::cap_tool_output(&result, tool_output_limit),
            ));
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
            0,
        )
        .await;

        assert_eq!(out.unwrap(), "Done: 42");
    }

    /// A malformed `<tool_use>` must be handed back for correction — the same
    /// contract as the main loop — not silently returned as an empty answer.
    #[tokio::test]
    async fn malformed_tool_call_is_retried_not_swallowed() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_responses(vec![
            "<tool_use>\n<name>shell</name>\n<arguments>\n{ not json }\n</arguments>\n</tool_use>"
                .to_string(),
            "Recovered".to_string(),
        ]));
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));

        let out = run_sub_agent(
            provider,
            tools,
            mcp,
            "system".to_string(),
            "do the thing".to_string(),
            "fake".to_string(),
            0.0,
            128,
            4,
            4,
            0,
        )
        .await;

        assert_eq!(out.unwrap(), "Recovered");
    }
}
