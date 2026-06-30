use crate::app::conversation::ConversationId;
use crate::app::events::{AgentResult, AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest, ToolCallInfo};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider};
use crate::tools::registry::ToolRegistry;
use crate::agent::tool_parser::{parse_tool_calls, stream_visible_text, strip_tool_calls};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Drive one agent turn for `conversation`. Every event it emits is tagged with
/// that id so the app routes it to the right conversation (focused or
/// background). When `auto_approve` is set (background sidechats / sub-agents,
/// where no user is watching) tool calls run without an approval prompt and
/// `question` calls are declined instead of blocking.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    conversation: ConversationId,
    provider: Arc<dyn LLMProvider>,
    tools: Arc<ToolRegistry>,
    mcp: Arc<tokio::sync::Mutex<MCPManager>>,
    messages: Vec<ChatMessage>,
    system_prompt: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    max_steps: usize,
    max_tools_per_step: usize,
    auto_approve: bool,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut collected_tool_calls = Vec::new();
    let mut messages = messages;

    for _step in 0..max_steps {
        let _ = event_tx.send(AppEvent::BeginAssistantMessage(conversation));
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
        if let Err(error) = provider.complete_stream(request, tx).await {
            let _ = event_tx.send(AppEvent::AgentError(conversation, error.to_string()));
            return;
        }

        let mut full_response = String::new();
        let mut streamed_visible = String::new();
        let idle_timeout = std::time::Duration::from_secs(120);
        loop {
            match tokio::time::timeout(idle_timeout, rx.recv()).await {
                Err(_) => {
                    let _ = event_tx.send(AppEvent::AgentError(
                        conversation,
                        "Stream timed out (no data for 120s). Cancelling turn.".to_string(),
                    ));
                    return;
                }
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    if !chunk.content.is_empty() {
                        full_response.push_str(&chunk.content);
                        let next_visible = stream_visible_text(&full_response);
                        if next_visible.starts_with(&streamed_visible) {
                            let delta = &next_visible[streamed_visible.len()..];
                            if !delta.is_empty() {
                                let _ = event_tx.send(AppEvent::AgentChunk(conversation, delta.to_string()));
                            }
                        } else if !next_visible.is_empty() {
                            let _ = event_tx.send(AppEvent::AddMessage(conversation, ChatMessage::system(
                                "⚠ Streaming sync issue — agent will continue",
                            )));
                        }
                        streamed_visible = next_visible;
                    }
                    if matches!(chunk.finish_reason.as_deref(), Some("stop")) {
                        break;
                    }
                }
            }
        }

        let tool_calls = parse_tool_calls(&full_response);
        let visible_text = strip_tool_calls(&full_response);

        if tool_calls.is_empty() {
            if !visible_text.is_empty() {
                messages.push(ChatMessage::assistant(&visible_text));
            } else {
                let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage(conversation));
            }

            let _ = event_tx.send(AppEvent::AgentDone(conversation, AgentResult {
                text: visible_text,
                tool_calls: collected_tool_calls,
            }));
            return;
        }

        messages.push(ChatMessage::assistant(&visible_text));
        if visible_text.is_empty() {
            let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage(conversation));
        }

        for tool_call in tool_calls.into_iter().take(max_tools_per_step) {
            let tool_id = uuid::Uuid::new_v4().to_string();

            let (tool_result, is_error) = if tool_call.name == "question" {
                // Background turns (auto_approve) have no user to answer.
                if auto_approve {
                    ("Cannot ask the user from a background agent.".to_string(), true)
                } else {
                let question_text = tool_call.arguments["question"]
                    .as_str()
                    .unwrap_or("(no question)");
                let options: Vec<String> = tool_call.arguments["options"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let allow_custom = tool_call.arguments["allow_custom"]
                    .as_bool()
                    .unwrap_or(false);

                let qs = QuestionState::new(
                    question_text.to_string(),
                    options,
                    allow_custom,
                );
                let request = QuestionRequest::new();
                let _ = event_tx
                    .send(AppEvent::RequestQuestion(request.clone(), qs));

                let _ = event_tx.send(AppEvent::ToolStarted {
                    conversation,
                    name: tool_call.name.clone(),
                });

                match request.wait().await {
                    Some(answer) if !answer.is_empty() => {
                        (format!("User answered: {answer}"), false)
                    }
                    _ => {
                        ("User cancelled the question".to_string(), true)
                    }
                }
                }
            } else if tool_call.name == "task" {
                // Only the foreground (interactive) loop may spawn sub-agents;
                // sidechats / sub-agents (auto_approve) cannot — depth limit 1.
                if auto_approve {
                    ("Nested sub-agents are not allowed.".to_string(), true)
                } else {
                    let prompt = tool_call.arguments["prompt"].as_str().unwrap_or("").trim().to_string();
                    let label = tool_call.arguments["description"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("sub-agent task")
                        .to_string();
                    let background = tool_call.arguments["background"].as_bool().unwrap_or(false);
                    if prompt.is_empty() {
                        ("The 'task' tool requires a non-empty 'prompt'.".to_string(), true)
                    } else if background {
                        let _ = event_tx.send(AppEvent::SpawnSubAgent {
                            parent: conversation,
                            label: label.clone(),
                            prompt,
                        });
                        (format!("Started background sub-agent: {label}"), false)
                    } else {
                        let _ = event_tx.send(AppEvent::ToolStarted {
                            conversation,
                            name: "task".to_string(),
                        });
                        let sub_provider = provider.fork();
                        match crate::agent::sub_agent::run_sub_agent(
                            sub_provider,
                            Arc::clone(&tools),
                            Arc::clone(&mcp),
                            system_prompt.clone(),
                            prompt,
                            model.clone(),
                            temperature,
                            max_tokens,
                            max_steps.min(8),
                            max_tools_per_step,
                        )
                        .await
                        {
                            Ok(text) => (text, false),
                            Err(e) => (format!("Sub-agent failed: {e}"), true),
                        }
                    }
                }
            } else {
                let approved = if auto_approve {
                    true
                } else {
                    let arguments_preview =
                        serde_json::to_string_pretty(&tool_call.arguments)
                            .unwrap_or_else(|_| tool_call.arguments.to_string());
                    let approval = ToolApprovalRequest::new(
                        tool_call.name.clone(),
                        arguments_preview,
                    );
                    let _ = event_tx
                        .send(AppEvent::RequestToolApproval(approval.clone()));
                    approval.wait().await
                };

                if approved {
                    let _ = event_tx.send(AppEvent::ToolStarted {
                        conversation,
                        name: tool_call.name.clone(),
                    });
                    if tool_call.name.starts_with("mcp__") {
                        let mut mcp = mcp.lock().await;
                        match mcp
                            .call_tool(
                                &tool_call.name,
                                tool_call.arguments.clone(),
                            )
                            .await
                        {
                            Ok(result) => (result.content, result.is_error),
                            Err(error) => (error.to_string(), true),
                        }
                    } else {
                        let result = tools
                            .execute(
                                &tool_call.name,
                                tool_call.arguments.clone(),
                            )
                            .await;
                        (result.content, result.is_error)
                    }
                } else {
                    ("Execution denied by user.".to_string(), true)
                }
            };

            let preview = summarize_tool_result(&tool_result);
            let display = preview.clone();

            let tool_message = ChatMessage::tool_with_display(
                &tool_id,
                &tool_call.name,
                &tool_result,
                &display,
                is_error,
            );
            messages.push(tool_message.clone());
            collected_tool_calls.push(ToolCallInfo {
                name: tool_call.name.clone(),
                arguments: tool_call.arguments.clone(),
                result: Some(tool_result.clone()),
            });
            let _ = event_tx.send(AppEvent::AddMessage(conversation, tool_message));

            if is_error {
                let _ = event_tx.send(AppEvent::ToolError {
                    conversation,
                    error: preview,
                });
            } else {
                let _ = event_tx.send(AppEvent::ToolDone {
                    conversation,
                    result: preview,
                });
            }
        }
    }

    let _ = event_tx.send(AppEvent::AgentError(
        conversation,
        "Reached max agent steps before producing a final answer".to_string(),
    ));
}

fn summarize_tool_result(result: &str) -> String {
    let trimmed = result.trim();
    if trimmed.len() <= 200 {
        trimmed.to_string()
    } else {
        // Find a safe char boundary at or before byte 200
        let end = trimmed.floor_char_boundary(200);
        format!("{}…", &trimmed[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    /// The provider seam is substitutable: with a `FakeProvider` the agent loop
    /// runs to completion — streaming visible text, then `AgentDone` — with no
    /// network, no proof-of-work, and no DeepSeek token.
    #[tokio::test]
    async fn agent_loop_streams_plain_response_then_done() {
        let provider: Arc<dyn LLMProvider> =
            Arc::new(FakeProvider::with_response("Hello, world!").chunked(3));
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            cid,
            provider,
            tools,
            mcp,
            vec![ChatMessage::user("hi")],
            "system".to_string(),
            "fake".to_string(),
            0.0,
            128,
            4,
            4,
            false,
            event_tx,
        )
        .await;

        let mut events = Vec::new();
        while let Ok(ev) = event_rx.try_recv() {
            events.push(ev);
        }

        // First event opens the assistant message, tagged with our conversation.
        assert!(matches!(events.first(), Some(AppEvent::BeginAssistantMessage(id)) if *id == cid));

        // The streamed deltas reassemble into the full visible response.
        let streamed: String = events
            .iter()
            .filter_map(|e| match e {
                AppEvent::AgentChunk(_, s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "Hello, world!");

        // The turn ends with AgentDone carrying the final text and no tool calls.
        let done = events
            .iter()
            .find_map(|e| match e {
                AppEvent::AgentDone(_, result) => Some(result),
                _ => None,
            })
            .expect("agent loop should finish with AgentDone");
        assert_eq!(done.text, "Hello, world!");
        assert!(done.tool_calls.is_empty());
    }

    #[test]
    fn summarize_short_result() {
        assert_eq!(summarize_tool_result("hello"), "hello");
    }

    #[test]
    fn summarize_whitespace_trimmed() {
        assert_eq!(summarize_tool_result("  hello  "), "hello");
    }

    #[test]
    fn summarize_exactly_200_bytes() {
        let input = "a".repeat(200);
        assert_eq!(summarize_tool_result(&input), input);
    }

    #[test]
    fn summarize_over_200_bytes() {
        let input = "a".repeat(250);
        let result = summarize_tool_result(&input);
        // Result should be at most 200 chars + 3-byte ellipsis
        assert!(result.len() <= 203);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn summarize_multibyte_safe() {
        // Each emoji is 4 bytes
        let input = "😀".repeat(60); // 240 bytes
        let result = summarize_tool_result(&input);
        // Should not panic on char boundary; result <= 200 + 3 (ellipsis)
        assert!(result.len() <= 203);
    }

    #[test]
    fn summarize_empty() {
        assert_eq!(summarize_tool_result(""), "");
    }
}
