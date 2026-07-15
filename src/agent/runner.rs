use crate::agent::stream::{StreamEnd, collect_stream};
use crate::agent::tool_parser::{
    StreamTextTracker, parse_tool_calls_with_errors, strip_tool_calls,
};
use crate::app::conversation::ConversationId;
use crate::app::events::{
    AgentResult, AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest, ToolCallInfo,
};
use crate::debug_log;
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use crate::semantic::SemanticService;
use crate::tools::registry::ToolRegistry;
use crate::tools::{QUESTION_TOOL_NAME, TASK_TOOL_NAME};
use serde_json::json;
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
    semantic: Arc<SemanticService>,
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
    let mut malformed_retries: u32 = 0;

    // Semantic hint: match the newest user message against the skill and
    // MCP-tool corpora and attach an advisory note the model can act on
    // (`skill` tool / listed MCP tools). Lives only in this turn's local
    // message copy — it is never persisted to the conversation. ONNX
    // inference is CPU-bound, so it runs on a blocking thread, not this
    // task; a not-yet-initialized matcher returns nothing instantly.
    //
    // The hint is inserted *before* the user message it annotates, never
    // after: providers that send only the newest tail (DeepSeek's flat
    // prompt) must always deliver the user's text as the final USER INPUT —
    // a trailing system note used to displace it entirely.
    if let Some(user_idx) = messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User) && !m.content.trim().is_empty())
    {
        let user_text = messages[user_idx].content.clone();
        let service = Arc::clone(&semantic);
        let matches = tokio::task::spawn_blocking(move || service.match_prompt(&user_text))
            .await
            .unwrap_or_default();
        if let Some(hint) = semantic.render_hint(&matches) {
            let described: Vec<String> =
                matches
                    .skills
                    .iter()
                    .map(|m| format!("skill:{}(d={:.3},s={:.3})", m.slug, m.dense, m.sparse))
                    .chain(matches.mcp_tools.iter().map(|m| {
                        format!("mcp:{}(d={:.3},s={:.3})", m.full_name, m.dense, m.sparse)
                    }))
                    .collect();
            debug_log::log(
                "agent.semantic_hint",
                format!(
                    "conversation={conversation} matches={}",
                    described.join(", ")
                ),
            );
            messages.insert(user_idx, ChatMessage::system(&hint));
        }
    }

    for step in 0..max_steps {
        let step_number = step + 1;
        debug_log::log(
            "agent.step.start",
            format!(
                "conversation={conversation} step={step_number}/{max_steps} message_count={}",
                messages.len()
            ),
        );
        let _ = event_tx.send(AppEvent::BeginAssistantMessage(conversation));
        let request =
            build_step_request(&system_prompt, &messages, &model, temperature, max_tokens);

        // Visible-delta emission rides the collector's progress callback:
        // strip tool-call syntax from the accumulated text and stream only
        // the newly-appended visible suffix. The tracker is per-step: the
        // accumulated text is append-only within one completion, which is
        // exactly what its incremental freezing relies on.
        let mut streamed_visible = String::new();
        let mut visible_tracker = StreamTextTracker::default();
        let progress_tx = event_tx.clone();
        let outcome = collect_stream(&provider, request, |full_response| {
            let next_visible = visible_tracker.visible(full_response);
            if next_visible.starts_with(&streamed_visible) {
                let delta = &next_visible[streamed_visible.len()..];
                if !delta.is_empty() {
                    let _ = progress_tx.send(AppEvent::AgentChunk(conversation, delta.to_string()));
                }
            } else if !next_visible.is_empty() {
                let _ = progress_tx.send(AppEvent::AddMessage(
                    conversation,
                    ChatMessage::system("⚠ Streaming sync issue — agent will continue"),
                ));
            }
            streamed_visible = next_visible;
        })
        .await;

        let full_response = outcome.text;
        let got_stop = outcome.got_stop;
        if matches!(outcome.end, StreamEnd::IdleTimeout) {
            debug_log::log(
                "agent.step.error",
                format!(
                    "conversation={conversation} step={step_number}/{max_steps} reason=stream_timeout"
                ),
            );
            let _ = event_tx.send(AppEvent::AgentError(
                conversation,
                "Stream timed out (no data for 120s). Cancelling turn.".to_string(),
            ));
            return;
        }
        if !got_stop {
            debug_log::log(
                "agent.step.stream_closed",
                format!(
                    "conversation={conversation} step={step_number}/{max_steps} got_stop={got_stop} collected_bytes={}",
                    full_response.len()
                ),
            );
        }

        // A closed channel without a "stop" chunk means the provider bailed
        // mid-stream — surface its error instead of treating it as end-of-turn.
        let mut provider_error_message: Option<String> = None;
        match outcome.end {
            StreamEnd::IdleTimeout => unreachable!("handled above"),
            StreamEnd::Completed => {
                if !got_stop {
                    let message = format!(
                        "Stream ended without stop signal. conversation={conversation} step={step_number}/{max_steps} response_bytes={}",
                        full_response.len()
                    );
                    debug_log::log("agent.step.error", &message);
                    provider_error_message = Some(message);
                } else {
                    debug_log::log(
                        "agent.step.provider_ok",
                        format!(
                            "conversation={conversation} step={step_number}/{max_steps} got_stop={got_stop} response_bytes={}",
                            full_response.len()
                        ),
                    );
                }
            }
            StreamEnd::ProviderError(message) => {
                debug_log::log(
                    "agent.step.error",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} reason=provider_error error={message}"
                    ),
                );
                if !got_stop {
                    provider_error_message = Some(message);
                }
            }
            StreamEnd::TaskFailed(join_error) => {
                debug_log::log(
                    "agent.step.error",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} reason=stream_task_join_error error={join_error}"
                    ),
                );
                if !got_stop {
                    provider_error_message = Some(format!("Stream task failed: {join_error}"));
                }
            }
        }

        let (tool_calls, parse_errors) = parse_tool_calls_with_errors(&full_response);
        let visible_text = strip_tool_calls(&full_response);
        debug_log::log(
            "agent.step.parsed",
            format!(
                "conversation={conversation} step={step_number}/{max_steps} got_stop={got_stop} visible_chars={} tool_calls={}",
                visible_text.chars().count(),
                tool_calls.len()
            ),
        );
        let tool_calls_for_log = tool_calls
            .iter()
            .map(|call| {
                json!({
                    "name": call.name,
                    "arguments": call.arguments,
                })
            })
            .collect::<Vec<_>>();
        debug_log::log_json(
            "agent.step.parsed.payload",
            &json!({
                "conversation": conversation.0,
                "step": step_number,
                "max_steps": max_steps,
                "got_stop": got_stop,
                "provider_error": provider_error_message,
                "full_response_chars": full_response.chars().count(),
                "full_response": full_response,
                "visible_text_chars": visible_text.chars().count(),
                "visible_text": visible_text,
                "tool_calls": tool_calls_for_log,
            }),
        );
        if let Some(error) = provider_error_message.take() {
            if !tool_calls.is_empty() {
                debug_log::log(
                    "agent.step.salvaged",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} reason=provider_error_with_complete_tool_call error={error} tool_calls={}",
                        tool_calls.len()
                    ),
                );
                let _ = event_tx.send(AppEvent::AddMessage(
                    conversation,
                    ChatMessage::system(
                        "Warning: stream ended early, but a complete tool call was recovered. Continuing.",
                    ),
                ));
            } else {
                let _ = event_tx.send(AppEvent::AgentError(conversation, error));
                return;
            }
        }

        if tool_calls.is_empty() {
            // A `<tool_use>` block that yielded zero parsed calls means the
            // model emitted a malformed tool call (swapped tags, invalid JSON,
            // …). Ending the turn silently here is exactly what made the agent
            // look frozen — nothing shown, nothing run. Feed the parse errors
            // back so it can re-issue the call, bounded so a model that can't
            // recover can't loop. (A provider error with empty calls already
            // returned above, so these errors are from a clean stream.)
            if !parse_errors.is_empty() && malformed_retries < MAX_MALFORMED_TOOL_RETRIES {
                malformed_retries += 1;
                // Keep what the model actually emitted (raw, not stripped) so
                // full-history providers see the mistake; DeepSeek already has
                // it server-side and re-sends only the correction below.
                messages.push(ChatMessage::assistant(&full_response));
                messages.push(ChatMessage::user(&malformed_tool_feedback(&parse_errors)));
                debug_log::log(
                    "agent.step.malformed_tool_use",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} retry={malformed_retries}/{MAX_MALFORMED_TOOL_RETRIES} errors={}",
                        parse_errors.join(" | ")
                    ),
                );
                let _ = event_tx.send(AppEvent::AddMessage(
                    conversation,
                    ChatMessage::system(&format!(
                        "⚠ Malformed tool call (attempt {malformed_retries}/{MAX_MALFORMED_TOOL_RETRIES}) — asking the model to retry"
                    )),
                ));
                continue;
            }
            if !parse_errors.is_empty() {
                debug_log::log(
                    "agent.step.malformed_tool_use_exhausted",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} errors={}",
                        parse_errors.join(" | ")
                    ),
                );
                let _ = event_tx.send(AppEvent::AddMessage(
                    conversation,
                    ChatMessage::system(
                        "⚠ The model kept emitting malformed tool calls; stopping this turn. Try rephrasing your request.",
                    ),
                ));
            }
            if !visible_text.is_empty() {
                messages.push(ChatMessage::assistant(&visible_text));
            } else {
                debug_log::log(
                    "agent.step.empty_assistant",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} reason=empty_response_without_tool_calls"
                    ),
                );
                let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage(conversation));
            }

            debug_log::log(
                "agent.turn.done",
                format!(
                    "conversation={conversation} step={step_number}/{max_steps} status=success text_chars={} tool_calls_total={}",
                    visible_text.chars().count(),
                    collected_tool_calls.len()
                ),
            );
            let _ = event_tx.send(AppEvent::AgentDone(
                conversation,
                AgentResult {
                    text: visible_text,
                    tool_calls: collected_tool_calls,
                },
            ));
            return;
        }

        messages.push(ChatMessage::assistant(&visible_text));
        if visible_text.is_empty() {
            let _ = event_tx.send(AppEvent::DiscardEmptyAssistantMessage(conversation));
        }

        let total_calls = tool_calls.len();
        for (call_index, tool_call) in tool_calls.into_iter().enumerate() {
            let tool_id = uuid::Uuid::new_v4().to_string();
            let tool_name = tool_call.name.clone();
            debug_log::log(
                "agent.tool.call",
                format!(
                    "conversation={conversation} step={step_number}/{max_steps} index={}/{} name={}",
                    call_index + 1,
                    total_calls,
                    tool_name
                ),
            );
            debug_log::log_json(
                "agent.tool.call.payload",
                &json!({
                    "conversation": conversation.0,
                    "step": step_number,
                    "max_steps": max_steps,
                    "tool_name": tool_name,
                    "call_index": call_index + 1,
                    "total_calls": total_calls,
                    "arguments": tool_call.arguments,
                }),
            );

            // Calls beyond the per-step limit still get a tool_result — the
            // model must learn they were skipped, not reason from phantom
            // results it assumes came back.
            if call_index >= max_tools_per_step {
                let skipped = tool_skip_message(max_tools_per_step, total_calls);
                let tool_message = ChatMessage::tool_with_display(
                    &tool_id,
                    &tool_call.name,
                    &skipped,
                    &summarize_tool_result(&skipped),
                    true,
                );
                messages.push(tool_message.clone());
                let _ = event_tx.send(AppEvent::AddMessage(conversation, tool_message));
                debug_log::log(
                    "agent.tool.skipped",
                    format!(
                        "conversation={conversation} step={step_number}/{max_steps} name={} reason=max_tools_per_step",
                        tool_name
                    ),
                );
                continue;
            }

            let (tool_result, is_error) = if tool_call.name == QUESTION_TOOL_NAME {
                // Background turns (auto_approve) have no user to answer.
                if auto_approve {
                    (
                        "Cannot ask the user from a background agent.".to_string(),
                        true,
                    )
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

                    let qs = QuestionState::new(question_text.to_string(), options, allow_custom);
                    let request = QuestionRequest::new();
                    let _ = event_tx.send(AppEvent::RequestQuestion(request.clone(), qs));

                    let _ = event_tx.send(AppEvent::ToolStarted {
                        conversation,
                        name: tool_call.name.clone(),
                    });

                    match request.wait().await {
                        Some(answer) if !answer.is_empty() => {
                            (format!("User answered: {answer}"), false)
                        }
                        _ => ("User cancelled the question".to_string(), true),
                    }
                }
            } else if tool_call.name == TASK_TOOL_NAME {
                // Only the foreground (interactive) loop may spawn sub-agents;
                // sidechats / sub-agents (auto_approve) cannot — depth limit 1.
                if auto_approve {
                    ("Nested sub-agents are not allowed.".to_string(), true)
                } else {
                    let prompt = tool_call.arguments["prompt"]
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let label = tool_call.arguments["description"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("sub-agent task")
                        .to_string();
                    let background = tool_call.arguments["background"].as_bool().unwrap_or(false);
                    if prompt.is_empty() {
                        (
                            "The 'task' tool requires a non-empty 'prompt'.".to_string(),
                            true,
                        )
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
                            name: TASK_TOOL_NAME.to_string(),
                        });
                        let sub_provider = provider.fork();
                        let session_cleanup = Arc::clone(&sub_provider);
                        let outcome = match crate::agent::sub_agent::run_sub_agent(
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
                        };
                        // The fork is one-shot — delete its server-side
                        // session so it doesn't linger on the account.
                        tokio::spawn(async move {
                            let _ = session_cleanup.discard_remote_session().await;
                        });
                        outcome
                    }
                }
            } else {
                let approved = if auto_approve {
                    true
                } else {
                    let arguments_preview = serde_json::to_string_pretty(&tool_call.arguments)
                        .unwrap_or_else(|_| tool_call.arguments.to_string());
                    let approval = ToolApprovalRequest::new(
                        conversation,
                        tool_call.name.clone(),
                        arguments_preview,
                    );
                    let _ = event_tx.send(AppEvent::RequestToolApproval(approval.clone()));
                    approval.wait().await
                };

                if approved {
                    let _ = event_tx.send(AppEvent::ToolStarted {
                        conversation,
                        name: tool_call.name.clone(),
                    });
                    dispatch_generic_tool(
                        &tools,
                        &mcp,
                        &tool_call.name,
                        tool_call.arguments.clone(),
                    )
                    .await
                } else {
                    ("Execution denied by user.".to_string(), true)
                }
            };

            let preview = summarize_tool_result(&tool_result);
            let display = preview.clone();
            debug_log::log(
                "agent.tool.result",
                format!(
                    "conversation={conversation} step={step_number}/{max_steps} name={} is_error={} result_chars={} preview={}",
                    tool_name,
                    is_error,
                    tool_result.chars().count(),
                    preview
                ),
            );
            debug_log::log_json(
                "agent.tool.result.payload",
                &json!({
                    "conversation": conversation.0,
                    "step": step_number,
                    "max_steps": max_steps,
                    "tool_name": tool_name,
                    "is_error": is_error,
                    "result_chars": tool_result.chars().count(),
                    "result": tool_result,
                    "preview": preview,
                }),
            );

            let tool_message = ChatMessage::tool_with_display(
                &tool_id,
                &tool_name,
                &tool_result,
                &display,
                is_error,
            );
            messages.push(tool_message.clone());
            collected_tool_calls.push(ToolCallInfo {
                name: tool_name.clone(),
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

    debug_log::log(
        "agent.turn.error",
        format!(
            "conversation={conversation} status=max_steps_exceeded max_steps={max_steps} tool_calls_total={}",
            collected_tool_calls.len()
        ),
    );
    let _ = event_tx.send(AppEvent::AgentError(
        conversation,
        "Reached max agent steps before producing a final answer".to_string(),
    ));
}

/// Build one step's completion request: system prompt first, then the
/// running history. Shared by the main and sub-agent loops (the two copies
/// had already been flagged as drift-prone).
pub(crate) fn build_step_request(
    system_prompt: &str,
    messages: &[ChatMessage],
    model: &str,
    temperature: f32,
    max_tokens: u32,
) -> CompletionRequest {
    let mut request_messages = Vec::with_capacity(messages.len() + 1);
    request_messages.push(ChatMessage::system(system_prompt));
    request_messages.extend(messages.iter().cloned());
    CompletionRequest {
        messages: request_messages,
        model: model.to_string(),
        temperature,
        max_tokens,
        stream: true,
    }
}

/// The explicit tool_result for a call dropped by the per-step limit — the
/// model must learn it was skipped, not reason from phantom results it
/// assumes came back. Was byte-identical in both loops before extraction.
pub(crate) fn tool_skip_message(max_tools_per_step: usize, total_calls: usize) -> String {
    format!(
        "Skipped: per-step tool-call limit of {max_tools_per_step} reached \
        ({total_calls} calls requested). Re-issue this call next step if still needed."
    )
}

/// Execute a plain (non-meta) tool call. MCP tools resolve their client
/// under a short-lived manager lock and call on the owned handle — holding
/// the mutex across the network await froze the whole UI once (the event
/// loop polls the same mutex for status counts); everything else goes to
/// the builtin registry. Returns `(content, is_error)`; the sub-agent loop
/// drops the flag and reports errors as plain result text.
pub(crate) async fn dispatch_generic_tool(
    tools: &ToolRegistry,
    mcp: &tokio::sync::Mutex<MCPManager>,
    name: &str,
    arguments: serde_json::Value,
) -> (String, bool) {
    if name.starts_with(crate::mcp::MCP_TOOL_PREFIX) {
        let client = { mcp.lock().await.client_for(name) };
        match client {
            Some((client, bare_name)) => match client.call_tool(&bare_name, arguments).await {
                Ok(result) => (result.content, result.is_error),
                Err(error) => (error.to_string(), true),
            },
            None => (
                format!("MCP tool '{name}' is not available (server not connected)"),
                true,
            ),
        }
    } else {
        let result = tools.execute(name, arguments).await;
        (result.content, result.is_error)
    }
}

/// How many times, within one turn, a malformed `<tool_use>` block may be
/// handed back to the model for correction before the turn gives up. Bounds
/// a weaker model that can't recover its own XML/JSON so it can't spin.
/// Shared with the sub-agent loop, which applies the same recovery.
pub(crate) const MAX_MALFORMED_TOOL_RETRIES: u32 = 2;

/// Correction message handed back to the model after it emitted a `<tool_use>`
/// block that couldn't be parsed. Names each concrete problem and restates the
/// exact required shape so the model can re-issue the call. Shared by the
/// main and sub-agent loops.
pub(crate) fn malformed_tool_feedback(errors: &[String]) -> String {
    format!(
        "Your previous message contained a tool call that could NOT be parsed, \
         so it was NOT executed:\n- {}\n\nRe-issue it using exactly this shape, \
         and nothing after </tool_use>:\n\
         <tool_use>\n<name>TOOL_NAME</name>\n<arguments>\n{{ \"key\": \"value\" }}\n</arguments>\n</tool_use>\n\n\
         The arguments must be a single valid JSON object. Inside JSON string \
         values, escape every backslash as \\\\ and every double quote as \\\" \
         (e.g. a Windows path: \"C:\\\\Users\\\\Aver\\\\file.png\").",
        errors.join("\n- ")
    )
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
            SemanticService::disabled(),
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

    #[tokio::test]
    async fn agent_loop_reports_error_when_stream_ends_without_stop() {
        let provider: Arc<dyn LLMProvider> = Arc::new(
            FakeProvider::with_response("partial")
                .chunked(2)
                .abrupt_eof(),
        );
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            cid,
            provider,
            tools,
            mcp,
            SemanticService::disabled(),
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

        let mut saw_done = false;
        let mut saw_error = None;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                AppEvent::AgentDone(_, _) => saw_done = true,
                AppEvent::AgentError(id, message) if id == cid => saw_error = Some(message),
                _ => {}
            }
        }

        assert!(!saw_done, "abrupt EOF must not look like a successful turn");
        let error = saw_error.expect("abrupt EOF should surface as AgentError");
        assert!(error.contains("without stop"));
    }

    #[tokio::test]
    async fn agent_loop_salvages_complete_tool_call_when_stream_ends_early() {
        let response = r#"<tool_use><name>question</name><arguments>{"question":"q","options":[],"allow_custom":false}</arguments></tool_use>"#;
        let provider: Arc<dyn LLMProvider> =
            Arc::new(FakeProvider::with_response(response).abrupt_eof());
        let tools = Arc::new(ToolRegistry::new());
        let mcp = Arc::new(tokio::sync::Mutex::new(MCPManager::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let cid = ConversationId::next();

        run_agent_loop(
            cid,
            provider,
            tools,
            mcp,
            SemanticService::disabled(),
            vec![ChatMessage::user("hi")],
            "system".to_string(),
            "fake".to_string(),
            0.0,
            128,
            4,
            4,
            true,
            event_tx,
        )
        .await;

        let mut saw_done = None;
        let mut saw_error = None;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                AppEvent::AgentDone(id, result) if id == cid => saw_done = Some(result),
                AppEvent::AgentError(id, message) if id == cid => saw_error = Some(message),
                _ => {}
            }
        }

        assert!(saw_error.is_none(), "complete tool call should be salvaged");
        let done = saw_done.expect("salvaged tool call should still finish the turn");
        assert_eq!(done.tool_calls.len(), 1);
        assert_eq!(done.tool_calls[0].name, "question");
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
