//! Исполнение вызовов инструментов одного шага. Отделено от `runner.rs`:
//! цикл меняется от политики повторов, а это — от новых мета-инструментов.

use crate::agent::sub_agent::SubAgentSpec;
use crate::agent::tool_parser::ParsedToolCall;
use crate::agent::trace::{StepTrace, ToolResultTrace};
use crate::app::conversation::ConversationId;
use crate::app::events::{
    AgentEvent, AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest, ToolCallInfo,
};
use crate::mcp::MCPManager;
use crate::provider::{ChatMessage, LLMProvider};
use crate::tools::registry::ToolRegistry;
use crate::tools::timer::{Timer, resolve_due};
use crate::tools::{QUESTION_TOOL_NAME, TASK_TOOL_NAME, TIMER_TOOL_NAME};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Что вернул инструмент. Кортеж `(String, bool)` протаскивался через пять
/// функций и читался на месте как `.0` и `.1`.
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) is_error: bool,
}

impl ToolOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    fn failed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

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
        // Идентификатор провайдера, если вызов пришёл родным протоколом:
        // результат обязан сослаться на тот же, иначе строгий эндпоинт
        // отвергнет следующий запрос. Свой придумываем только на промптовом
        // пути, где его нет вовсе.
        let tool_id = call
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let name = call.name.clone();
        // Единственная копия аргументов: одна уходит в исполнение, другая — в
        // запись хода. Раньше их было две, обе с полным телом вызова.
        let arguments = call.arguments.clone();
        trace.tool_call(index + 1, total, &name, &arguments);

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

        let ToolOutcome { text, is_error } = execute_tool_call(call, ctx).await;
        // Для большинства инструментов в ленту идёт выжимка: вывод бывает
        // огромным. Но у некоторых результат сам адресован человеку, и
        // обрезка съела бы всё, что в нём есть.
        let whole = ctx
            .tools
            .get(&name)
            .is_some_and(|tool| tool.result_is_its_own_summary());
        let preview = if whole {
            text.clone()
        } else {
            summarize_tool_result(&text)
        };
        // Модель получает урезанный вывод (ступень 0 ладдера), трасса — целый.
        let for_model = crate::context::cap_tool_output(&text, ctx.tool_output_limit);
        let capped = ctx.tool_output_limit != 0 && text.chars().count() > ctx.tool_output_limit;
        trace.tool_result(&ToolResultTrace {
            name: &name,
            is_error,
            result: &text,
            preview: &preview,
            capped,
            chars_sent: for_model.chars().count(),
        });

        let message =
            ChatMessage::tool_with_display(&tool_id, &name, &for_model, &preview, is_error);
        messages.push(message.clone());
        collected.push(ToolCallInfo {
            name,
            arguments,
            result: Some(text),
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

    /// «Инструмент пошёл»: три ветки исполнения шлют это одинаково, каждая
    /// перед своей работой.
    fn emit_tool_started(&self, name: &str) {
        self.emit(AgentEvent::ToolStarted {
            name: name.to_string(),
        });
    }
}

/// Развилка по имени вызова. Четыре ветки живут по своим причинам: вопрос —
/// это UX, `task` — запуск суб-агента, `timer` — состояние беседы, а
/// остальное — аппрув и диспетчер.
async fn execute_tool_call(call: ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    match call.name.as_str() {
        QUESTION_TOOL_NAME => ask_the_user(&call, ctx).await,
        TASK_TOOL_NAME => spawn_task(&call, ctx).await,
        TIMER_TOOL_NAME => {
            ctx.emit_tool_started(TIMER_TOOL_NAME);
            manage_timer(&call, ctx)
        }
        _ => run_generic_tool(call, ctx).await,
    }
}

/// `timer`: отложенная задача этой беседы. Фоновому ходу отказ — беседа
/// суб-агента исчезает вместе с его ответом, её таймер осиротеет.
fn manage_timer(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    if ctx.auto_approve {
        return ToolOutcome::failed("Timers are not available to background agents.");
    }
    let timers = ctx.tools.timers();
    let owner = ctx.conversation.0;
    let now = chrono::Local::now();

    match call.arguments["action"].as_str().unwrap_or("set") {
        "list" => {
            let pending = timers.list(Some(owner));
            if pending.is_empty() {
                return ToolOutcome::ok("No timers pending in this chat.");
            }
            let now = now.with_timezone(&chrono::Utc);
            ToolOutcome::ok(
                pending
                    .iter()
                    .map(|t| t.describe(now))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        "cancel" => match read_timer_id(call) {
            Some(id) => match timers.cancel(id, Some(owner)) {
                Some(timer) => ToolOutcome::ok(format!("Cancelled timer #{}: {}", id, timer.note)),
                None => ToolOutcome::failed(format!("No timer #{id} in this chat.")),
            },
            None => ToolOutcome::failed("Cancelling a timer needs its numeric 'id'."),
        },
        "set" => set_timer(call, &timers, owner, now),
        other => ToolOutcome::failed(format!(
            "Unknown timer action '{other}'. Use 'set', 'list', or 'cancel'."
        )),
    }
}

/// Взвести таймер и рассказать модели, когда он сработает: текущего времени
/// в промпте нет, так что абсолютную дату она узнаёт только отсюда.
fn set_timer(
    call: &ParsedToolCall,
    timers: &crate::tools::timer::TimerStore,
    owner: u64,
    now: chrono::DateTime<chrono::Local>,
) -> ToolOutcome {
    let note = call.arguments["note"].as_str().unwrap_or_default();
    let wake = call.arguments["wake"].as_bool().unwrap_or(false);
    let due = match resolve_due(
        now,
        call.arguments["after"].as_str(),
        call.arguments["at"].as_str(),
    ) {
        Ok(due) => due,
        Err(error) => return ToolOutcome::failed(error),
    };
    match timers.set(owner, due, note, wake) {
        Ok(timer) => ToolOutcome::ok(timer_set_message(&timer, now.with_timezone(&chrono::Utc))),
        Err(error) => ToolOutcome::failed(error),
    }
}

/// Ответ на взведённый таймер. Отдельно от `set_timer`, чтобы формулировку
/// проверял тест, а не глаз.
pub(crate) fn timer_set_message(timer: &Timer, now: chrono::DateTime<chrono::Utc>) -> String {
    let tail = if timer.wake {
        "It will start a turn here and hand you the note."
    } else {
        "It will show the note here; you are not resumed."
    };
    format!(
        "Timer set — {}\n{tail} Timers are lost if the app exits.",
        timer.describe(now)
    )
}

/// Модели свойственно слать число строкой — читаем оба вида.
fn read_timer_id(call: &ParsedToolCall) -> Option<u64> {
    let id = &call.arguments["id"];
    id.as_u64()
        .or_else(|| id.as_str().and_then(|s| s.trim().parse().ok()))
}

/// `question`: спросить человека. В фоновом ходе отвечать некому.
async fn ask_the_user(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    if ctx.auto_approve {
        return ToolOutcome::failed("Cannot ask the user from a background agent.");
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
    ctx.emit_tool_started(&call.name);
    match request.wait().await {
        Some(answer) if !answer.is_empty() => ToolOutcome::ok(format!("User answered: {answer}")),
        _ => ToolOutcome::failed("User cancelled the question"),
    }
}

/// `task`: запустить суб-агента. Глубина ограничена единицей — вложенных
/// суб-агентов нет, поэтому фоновому ходу здесь отказ.
async fn spawn_task(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    if ctx.auto_approve {
        return ToolOutcome::failed("Nested sub-agents are not allowed.");
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
        return ToolOutcome::failed("The 'task' tool requires a non-empty 'prompt'.");
    }
    if background {
        let _ = ctx.event_tx.send(AppEvent::SpawnSubAgent {
            parent: ctx.conversation,
            label: label.clone(),
            prompt,
        });
        return ToolOutcome::ok(format!("Started background sub-agent: {label}"));
    }
    ctx.emit_tool_started(TASK_TOOL_NAME);
    let sub_provider = ctx.provider.fork();
    let session_cleanup = Arc::clone(&sub_provider);
    let outcome = match crate::agent::sub_agent::run_sub_agent(SubAgentSpec {
        provider: sub_provider,
        tools: Arc::clone(ctx.tools),
        mcp: Arc::clone(ctx.mcp),
        system_prompt: ctx.system_prompt.to_string(),
        user_prompt: prompt,
        model: ctx.model.to_string(),
        temperature: ctx.temperature,
        max_tokens: ctx.max_tokens,
        max_steps: ctx.max_steps.min(8),
        max_tools_per_step: ctx.max_tools_per_step,
        tool_output_limit: ctx.tool_output_limit,
    })
    .await
    {
        Ok(text) => ToolOutcome::ok(text),
        Err(e) => ToolOutcome::failed(format!("Sub-agent failed: {e}")),
    };
    // Форк одноразовый — снести его серверную сессию, чтобы не копилась.
    tokio::spawn(async move {
        let _ = session_cleanup.discard_remote_session().await;
    });
    outcome
}

/// Обычный инструмент: аппрув, затем диспетчер (builtin или MCP).
async fn run_generic_tool(call: ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    // Незнакомое имя (в том числе любой MCP-инструмент) подтверждаем всегда:
    // отказаться от модалки может только тот, кто сам объявил, что ему нечего
    // подтверждать.
    let needs_approval = ctx
        .tools
        .get(&call.name)
        .is_none_or(|tool| tool.requires_approval());
    let approved = if ctx.auto_approve || !needs_approval {
        true
    } else {
        // Не сырой JSON: он экранирует переводы строк, и содержимое файла в
        // модалке становится одной нечитаемой строкой.
        let arguments_preview = crate::tools::approval_preview(&call.name, &call.arguments);
        let approval = ToolApprovalRequest::new(
            ctx.conversation,
            call.name.clone(),
            arguments_preview,
            crate::tools::approval_scope(&call.name, &call.arguments),
        );
        let _ = ctx
            .event_tx
            .send(AppEvent::RequestToolApproval(approval.clone()));
        approval.wait().await
    };
    if !approved {
        return ToolOutcome::failed("Execution denied by user.");
    }
    ctx.emit_tool_started(&call.name);
    dispatch_generic_tool(ctx.tools, ctx.mcp, &call.name, call.arguments).await
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
) -> ToolOutcome {
    if name.starts_with(crate::mcp::MCP_TOOL_PREFIX) {
        let client = { mcp.lock().await.client_for(name) };
        match client {
            Some((client, bare_name)) => match client.call_tool(&bare_name, arguments).await {
                Ok(result) => ToolOutcome {
                    text: result.content,
                    is_error: result.is_error,
                },
                Err(error) => ToolOutcome::failed(error.to_string()),
            },
            None => ToolOutcome::failed(format!(
                "MCP tool '{name}' is not available (server not connected)"
            )),
        }
    } else {
        let result = tools.execute(name, arguments).await;
        ToolOutcome {
            text: result.content,
            is_error: result.is_error,
        }
    }
}

/// Короткая выжимка для строки состояния и для трассы. Режет по границе
/// символа через общий помощник (инвариант 4), а не своей копией.
fn summarize_tool_result(result: &str) -> String {
    const PREVIEW_BYTES: usize = 200;
    let trimmed = result.trim();
    let head = crate::util::truncate_at_char_boundary(trimmed, PREVIEW_BYTES);
    if head.len() == trimmed.len() {
        head.to_string()
    } else {
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_result_is_kept_whole() {
        assert_eq!(summarize_tool_result("hello"), "hello");
    }

    #[test]
    fn surrounding_whitespace_is_dropped() {
        assert_eq!(summarize_tool_result("  hello  "), "hello");
    }

    #[test]
    fn exactly_the_limit_is_not_marked_as_cut() {
        let input = "a".repeat(200);
        assert_eq!(summarize_tool_result(&input), input);
    }

    #[test]
    fn a_longer_result_is_cut_and_marked() {
        let result = summarize_tool_result(&"a".repeat(250));
        assert!(result.len() <= 203);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn a_cut_never_splits_a_multibyte_char() {
        // Каждая эмодзи занимает четыре байта.
        let result = summarize_tool_result(&"😀".repeat(60));
        assert!(result.len() <= 203);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn an_empty_result_stays_empty() {
        assert_eq!(summarize_tool_result(""), "");
    }
}
