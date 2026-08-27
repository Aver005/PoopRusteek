//! Исполнение вызовов инструментов одного шага. Отделено от `runner.rs`:
//! цикл меняется от политики повторов, а это — от новых мета-инструментов.

use crate::agent::tool_parser::ParsedToolCall;
use crate::agent::trace::StepTrace;
use crate::app::conversation::ConversationId;
use crate::app::events::{
    AgentEvent, AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest, ToolCallInfo,
};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider};
use crate::tools::registry::ToolRegistry;
use crate::tools::{QUESTION_TOOL_NAME, TASK_TOOL_NAME};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Прогнать вызовы шага. Пропущенный по лимиту всё равно получает
/// `tool_result`: иначе модель рассуждает по несуществующему ответу.
pub(super) async fn run_tool_calls(
    calls: Vec<ParsedToolCall>,
    ctx: &ToolExecContext<'_>,
    trace: &StepTrace,
    messages: &mut Vec<ChatMessage>,
    collected: &mut Vec<ToolCallInfo>,
) {
    let total = calls.len();
    for (index, call) in calls.into_iter().enumerate() {
        let tool_id = uuid::Uuid::new_v4().to_string();
        let name = call.name.clone();
        trace.tool_call(index + 1, total, &name);
        trace.tool_call_payload(index + 1, total, &name, &call.arguments);

        if index >= ctx.max_tools_per_step {
            let skipped = tool_skip_message(ctx.max_tools_per_step, total);
            let message = ChatMessage::tool_with_display(
                &tool_id,
                &name,
                &skipped,
                &summarize_tool_result(&skipped),
                true,
            );
            messages.push(message.clone());
            ctx.emit(AgentEvent::Message(message));
            trace.tool_skipped(&name);
            continue;
        }

        let (result, is_error) = execute_tool_call(&call, ctx).await;
        let preview = summarize_tool_result(&result);
        // Модель получает урезанный вывод (ступень 0 ладдера), трасса — целый.
        let for_model = crate::context::cap_tool_output(&result, ctx.tool_output_limit);
        let capped = ctx.tool_output_limit != 0 && result.chars().count() > ctx.tool_output_limit;
        trace.tool_result(&name, is_error, &result, &preview);
        trace.tool_result_payload(
            &name,
            is_error,
            &result,
            &preview,
            capped,
            for_model.chars().count(),
        );

        let message =
            ChatMessage::tool_with_display(&tool_id, &name, &for_model, &preview, is_error);
        messages.push(message.clone());
        collected.push(ToolCallInfo {
            name,
            arguments: call.arguments.clone(),
            result: Some(result),
        });
        ctx.emit(AgentEvent::Message(message));
        ctx.emit(if is_error {
            AgentEvent::ToolError { error: preview }
        } else {
            AgentEvent::ToolDone { result: preview }
        });
    }
}

/// Всё, что нужно одному вызову от хода. Иначе дюжина позиционных
/// аргументов — та же болезнь, что `TurnSpec` лечит уровнем выше.
pub(super) struct ToolExecContext<'turn> {
    pub(super) conversation: ConversationId,
    pub(super) provider: &'turn Arc<dyn LLMProvider>,
    pub(super) tools: &'turn Arc<ToolRegistry>,
    pub(super) mcp: &'turn Arc<tokio::sync::Mutex<MCPManager>>,
    pub(super) system_prompt: &'turn str,
    pub(super) model: &'turn str,
    pub(super) temperature: f32,
    pub(super) max_tokens: u32,
    pub(super) max_steps: usize,
    pub(super) max_tools_per_step: usize,
    pub(super) auto_approve: bool,
    pub(super) tool_output_limit: usize,
    pub(super) event_tx: &'turn mpsc::UnboundedSender<AppEvent>,
}

impl ToolExecContext<'_> {
    /// Событие этого хода. Обёртка вокруг `AppEvent::Agent` — в цикле шага
    /// она встречается десяток раз.
    pub(super) fn emit(&self, event: AgentEvent) {
        let _ = self.event_tx.send(AppEvent::Agent {
            conversation: self.conversation,
            event,
        });
    }
}

/// Развилка по имени вызова. Три ветки живут по своим причинам: вопрос —
/// это UX, `task` — запуск суб-агента, остальное — аппрув и диспетчер.
async fn execute_tool_call(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> (String, bool) {
    match call.name.as_str() {
        QUESTION_TOOL_NAME => ask_the_user(call, ctx).await,
        TASK_TOOL_NAME => spawn_task(call, ctx).await,
        _ => run_generic_tool(call, ctx).await,
    }
}

/// `question`: спросить человека. В фоновом ходе спрашивать некого.
async fn ask_the_user(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> (String, bool) {
    {
        // В фоновом ходе отвечать некому.
        if ctx.auto_approve {
            return (
                "Cannot ask the user from a background agent.".to_string(),
                true,
            );
        }
        let question_text = call.arguments["question"]
            .as_str()
            .unwrap_or("(no question)");
        let options: Vec<String> = call.arguments["options"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let allow_custom = call.arguments["allow_custom"].as_bool().unwrap_or(false);

        let qs = QuestionState::new(question_text.to_string(), options, allow_custom);
        let request = QuestionRequest::new();
        let _ = ctx
            .event_tx
            .send(AppEvent::RequestQuestion(request.clone(), qs));
        let _ = ctx.event_tx.send(AppEvent::Agent {
            conversation: ctx.conversation,
            event: AgentEvent::ToolStarted {
                name: call.name.clone(),
            },
        });
        match request.wait().await {
            Some(answer) if !answer.is_empty() => (format!("User answered: {answer}"), false),
            _ => ("User cancelled the question".to_string(), true),
        }
    }
}

/// `task`: запустить суб-агента. Глубина ограничена единицей — вложенных
/// суб-агентов нет, поэтому фоновому ходу здесь отказ.
async fn spawn_task(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> (String, bool) {
    {
        // Суб-агента заводит только передний план: глубина ограничена 1.
        if ctx.auto_approve {
            return ("Nested sub-agents are not allowed.".to_string(), true);
        }
        let prompt = call.arguments["prompt"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let label = call.arguments["description"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("sub-agent task")
            .to_string();
        let background = call.arguments["background"].as_bool().unwrap_or(false);
        if prompt.is_empty() {
            return (
                "The 'task' tool requires a non-empty 'prompt'.".to_string(),
                true,
            );
        }
        if background {
            let _ = ctx.event_tx.send(AppEvent::SpawnSubAgent {
                parent: ctx.conversation,
                label: label.clone(),
                prompt,
            });
            return (format!("Started background sub-agent: {label}"), false);
        }
        let _ = ctx.event_tx.send(AppEvent::Agent {
            conversation: ctx.conversation,
            event: AgentEvent::ToolStarted {
                name: TASK_TOOL_NAME.to_string(),
            },
        });
        let sub_provider = ctx.provider.fork();
        let session_cleanup = Arc::clone(&sub_provider);
        let outcome = match crate::agent::sub_agent::run_sub_agent(
            sub_provider,
            Arc::clone(ctx.tools),
            Arc::clone(ctx.mcp),
            ctx.system_prompt.to_string(),
            prompt,
            ctx.model.to_string(),
            ctx.temperature,
            ctx.max_tokens,
            ctx.max_steps.min(8),
            ctx.max_tools_per_step,
            ctx.tool_output_limit,
        )
        .await
        {
            Ok(text) => (text, false),
            Err(e) => (format!("Sub-agent failed: {e}"), true),
        };
        // Форк одноразовый — снести его серверную сессию, чтобы не копилась.
        tokio::spawn(async move {
            let _ = session_cleanup.discard_remote_session().await;
        });
        outcome
    }
}

/// Обычный инструмент: аппрув, затем диспетчер (builtin или MCP).
async fn run_generic_tool(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> (String, bool) {
    let approved = if ctx.auto_approve {
        true
    } else {
        let arguments_preview = serde_json::to_string_pretty(&call.arguments)
            .unwrap_or_else(|_| call.arguments.to_string());
        let approval =
            ToolApprovalRequest::new(ctx.conversation, call.name.clone(), arguments_preview);
        let _ = ctx
            .event_tx
            .send(AppEvent::RequestToolApproval(approval.clone()));
        approval.wait().await
    };
    if !approved {
        return ("Execution denied by user.".to_string(), true);
    }
    let _ = ctx.event_tx.send(AppEvent::Agent {
        conversation: ctx.conversation,
        event: AgentEvent::ToolStarted {
            name: call.name.clone(),
        },
    });
    dispatch_generic_tool(ctx.tools, ctx.mcp, &call.name, call.arguments.clone()).await
}

/// Явный `tool_result` для вызова, срезанного лимитом шага.
/// Обоим циклам нужен одинаковый текст.
pub(crate) fn tool_skip_message(max_tools_per_step: usize, total_calls: usize) -> String {
    format!(
        "Skipped: per-step tool-call limit of {max_tools_per_step} reached \
        ({total_calls} calls requested). Re-issue this call next step if still needed."
    )
}

/// Обычный вызов. MCP-клиент достаётся под коротким локом и вызывается уже
/// на своём хэндле: лок через сетевой `await` однажды заморозил весь UI.
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
