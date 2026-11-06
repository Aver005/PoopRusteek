//! Исполнение вызовов инструментов одного шага. Отделено от `runner.rs`:
//! цикл меняется от политики повторов, а это — от новых мета-инструментов.

use crate::agent::tool_parser::ParsedToolCall;
use crate::agent::trace::{StepTrace, ToolResultTrace};
use crate::app::conversation::ConversationId;
use crate::app::events::{
    AgentEvent, AppEvent, QuestionRequest, QuestionState, ToolApprovalRequest, ToolCallInfo,
};
use crate::mcp::MCPManager;
use crate::mcp::manager::BROKEN_AFTER_FAILURES;
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
    pub(super) fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub(super) fn failed(text: impl Into<String>) -> Self {
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
        // Только для успешного результата: текст ошибки собирается из
        // аргументов модели и ничем не ограничен, а он уходит и в строку
        // состояния, и в постоянное сообщение истории.
        let whole = !is_error
            && ctx
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
    pub(super) unattended: bool,
    pub(super) tool_output_limit: usize,
    pub(super) event_tx: &'turn mpsc::UnboundedSender<AppEvent>,
    /// Суб-агенту `task` — тот же индекс подсказок и та же лестница компакции.
    pub(super) semantic: &'turn Arc<crate::semantic::SemanticService>,
    pub(super) context: &'turn crate::context::ContextSpec,
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
    fn emit_tool_started(&self, name: &str, arguments: &serde_json::Value) {
        self.emit(AgentEvent::ToolStarted {
            name: name.to_string(),
            summary: tool_call_summary(arguments),
        });
    }
}

/// Ключи аргументов, по которым узнаётся суть вызова, по убыванию важности.
const SUMMARY_KEYS: [&str; 8] = [
    "command",
    "cmd",
    "path",
    "file_path",
    "pattern",
    "query",
    "url",
    "prompt",
];
/// Длина строки сути вызова в символах.
const SUMMARY_CHARS: usize = 100;

/// Одна короткая строка про вызов для панели работы: команда, путь, запрос.
/// Значения секретных полей не берутся, а в самой строке секреты маскируются.
/// Пусто, если в аргументах нет ни одной подходящей строки.
pub(crate) fn tool_call_summary(arguments: &serde_json::Value) -> String {
    let text = SUMMARY_KEYS
        .iter()
        .find_map(|key| arguments.get(key).and_then(|value| value.as_str()))
        .or_else(|| {
            arguments
                .as_object()?
                .iter()
                .filter(|(name, _)| !super::redact::is_secret_name(name))
                .find_map(|(_, value)| value.as_str())
        })
        .unwrap_or_default();
    // Сначала маска, потом усечение: обрезка не должна оставить кусок секрета.
    crate::util::one_line_label(&super::redact::redact_secrets(text), SUMMARY_CHARS)
}

/// Человек посмотрел и отказал.
const DENIED_BY_USER: &str = "Execution denied by user.";

/// Спросить некого, а сам вызов одобрять без человека нельзя.
const UNATTENDED_FOREIGN_CALL: &str = "This call was written in a non-standard format (or with a corrected tool name), which only a person may approve, and nobody is watching this run. Re-send it as <tool_use><name>…</name><arguments>{…}</arguments></tool_use> with the exact tool name.";

/// Развилка по имени вызова. Четыре ветки живут по своим причинам: вопрос —
/// это UX, `task` — запуск суб-агента, `timer` — состояние беседы, а
/// остальное — аппрув и диспетчер.
async fn execute_tool_call(call: ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    if let Some(refusal) = refuse_non_object(&call.name, &call.arguments) {
        return refusal;
    }
    // Чужая разметка не обходит подтверждение и у особых инструментов:
    // `task` и `timer` иначе обошли бы проверку разметки.
    if call.origin.needs_person() && matches!(call.name.as_str(), TASK_TOOL_NAME | TIMER_TOOL_NAME)
    {
        if ctx.unattended {
            return ToolOutcome::failed(UNATTENDED_FOREIGN_CALL);
        }
        if !ask_person(&call, ctx, true).await {
            return ToolOutcome::failed(DENIED_BY_USER);
        }
    }
    match call.name.as_str() {
        QUESTION_TOOL_NAME => ask_the_user(&call, ctx).await,
        TASK_TOOL_NAME => spawn_task(&call, ctx).await,
        TIMER_TOOL_NAME => manage_timer(&call, ctx),
        _ => run_generic_tool(call, ctx).await,
    }
}

/// `timer`: отложенная задача этой беседы. Фоновому ходу отказ — беседа
/// суб-агента исчезает вместе с его ответом, её таймер осиротеет.
fn manage_timer(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    if ctx.unattended {
        return ToolOutcome::failed("Timers are not available to background agents.");
    }
    // «Пошёл» — только для правда запущенного: отказ сводка не считает.
    ctx.emit_tool_started(TIMER_TOOL_NAME, &call.arguments);
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
    if ctx.unattended {
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
    ctx.emit_tool_started(&call.name, &call.arguments);
    let answer = request.wait().await;
    if answer.is_empty() {
        ToolOutcome::failed("User cancelled the question")
    } else {
        ToolOutcome::ok(format!("User answered: {answer}"))
    }
}

/// `task`: запустить суб-агента. Глубина ограничена единицей — вложенных
/// суб-агентов нет, поэтому ходу без человека здесь отказ.
async fn spawn_task(call: &ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    if ctx.unattended {
        return ToolOutcome::failed(
            "Sub-agents are not available here: this is already a background or sub-agent run.",
        );
    }
    let prompt = call.arguments["prompt"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    // Подпись пишет модель, а показывается она в модалке и строке состояния.
    let label = call.arguments["description"]
        .as_str()
        .map(|text| crate::util::one_line_label(text, 60))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "sub-agent task".to_string());
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
        return ToolOutcome::ok(format!(
            "Started background sub-agent: {label}. Its result will be added to this conversation when it finishes; you are not woken up for it."
        ));
    }
    ctx.emit_tool_started(TASK_TOOL_NAME, &call.arguments);
    crate::agent::sub_agent::run_task(ctx, &label, prompt).await
}

/// Обычный инструмент: аппрув, затем диспетчер (builtin или MCP).
async fn run_generic_tool(call: ParsedToolCall, ctx: &ToolExecContext<'_>) -> ToolOutcome {
    // Незнакомое имя (в том числе любой MCP-инструмент) подтверждаем всегда:
    // отказаться от модалки может только тот, кто сам объявил, что ему нечего
    // подтверждать.
    let person = call.origin.needs_person();
    let needs_approval = person
        || ctx
            .tools
            .get(&call.name)
            .is_none_or(|tool| tool.requires_approval());
    // Обычно такой вызов без человека отвергает ещё разбор; дошёл — отказ.
    if person && ctx.unattended {
        return ToolOutcome::failed(UNATTENDED_FOREIGN_CALL);
    }
    if needs_approval && !ask_person(&call, ctx, person).await {
        return ToolOutcome::failed(DENIED_BY_USER);
    }
    ctx.emit_tool_started(&call.name, &call.arguments);
    dispatch_generic_tool(ctx.tools, ctx.mcp, &call.name, call.arguments).await
}

/// Запрос подтверждения. `always` — белый список не действует; команда про доверие и секреты
/// получает это сама, и причина стоит первой строкой окна.
fn build_approval(
    call: &ParsedToolCall,
    conversation: ConversationId,
    always: bool,
) -> ToolApprovalRequest {
    // Не сырой JSON: он экранирует переводы строк, и содержимое файла в
    // модалке становится одной нечитаемой строкой.
    let mut preview = crate::tools::approval_preview(&call.name, &call.arguments);
    let risky = crate::tools::sensitive_shell_reason(&call.name, &call.arguments);
    if let Some(reason) = risky {
        preview = format!("{reason}\n{preview}");
    }
    if let Some(warning) = call.origin.warning() {
        preview = format!("{warning}\n{preview}");
    }
    ToolApprovalRequest::new(
        conversation,
        call.name.clone(),
        preview,
        crate::tools::approval_scope(&call.name, &call.arguments),
    )
    .asking_always(always || risky.is_some())
}

/// Окно подтверждения.
async fn ask_person(call: &ParsedToolCall, ctx: &ToolExecContext<'_>, always: bool) -> bool {
    let approval = build_approval(call, ctx.conversation, always);
    let _ = ctx
        .event_tx
        .send(AppEvent::RequestToolApproval(approval.clone()));
    approval.wait().await
}

/// Явный `tool_result` для вызова, срезанного лимитом шага.
fn tool_skip_message(max_tools_per_step: usize, total_calls: usize) -> String {
    format!(
        "Skipped: per-step tool-call limit of {max_tools_per_step} reached \
        ({total_calls} calls requested). Re-issue this call next step if still needed."
    )
}

/// Обычный вызов. MCP-клиент достаётся под коротким локом и вызывается уже
/// на своём хэндле: лок через сетевой `await` однажды заморозил весь UI.
async fn dispatch_generic_tool(
    tools: &ToolRegistry,
    mcp: &tokio::sync::Mutex<MCPManager>,
    name: &str,
    arguments: serde_json::Value,
) -> ToolOutcome {
    if let Some(refusal) = refuse_non_object(name, &arguments) {
        return refusal;
    }
    if name.starts_with(crate::mcp::MCP_TOOL_PREFIX) {
        let target = { mcp.lock().await.client_for(name) };
        match target {
            Ok(target) => match target.client.call_tool(&target.tool, arguments).await {
                Ok(result) => ToolOutcome {
                    text: result.content,
                    is_error: result.is_error,
                },
                Err(error) => {
                    let reason = error.to_string();
                    if target.client.failures_in_row() < BROKEN_AFTER_FAILURES {
                        return ToolOutcome::failed(format!(
                            "MCP server '{}' failed: {reason}",
                            target.server
                        ));
                    }
                    mcp.lock().await.mark_broken(&target, &reason);
                    ToolOutcome::failed(format!(
                        "MCP server '{}' failed {BROKEN_AFTER_FAILURES} times in a row ({reason}) and is now marked broken; its tools will not work until the user reconnects it with /mcp.",
                        target.server
                    ))
                }
            },
            Err(unavailable) => ToolOutcome::failed(unavailable.explain(name)),
        }
    } else {
        let result = tools.execute(name, arguments).await;
        ToolOutcome {
            text: result.content,
            is_error: result.is_error,
        }
    }
}

/// Аргументы не объект: родной протокол хранит битый JSON строкой, разбор
/// пропускает массив. Исполнять нельзя, а «Missing 'path'» соврал бы о причине.
fn refuse_non_object(name: &str, arguments: &serde_json::Value) -> Option<ToolOutcome> {
    let reason = match arguments {
        serde_json::Value::Object(_) => return None,
        serde_json::Value::String(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            Err(error) => format!("they are not valid JSON: {error}"),
            Ok(_) => "they are a JSON string, not an object".to_string(),
        },
        other => format!("they must be a JSON object, got {other}"),
    };
    Some(ToolOutcome::failed(format!(
        "The arguments of this '{name}' call could not be used ({reason}), so it was not run. Re-send it with a JSON object."
    )))
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

    fn bash_call(command: &str) -> ParsedToolCall {
        ParsedToolCall {
            id: None,
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": command }),
            origin: Default::default(),
        }
    }

    #[test]
    fn a_trust_command_ignores_the_whitelist_and_says_why() {
        let everything =
            crate::whitelist::Whitelist::from_rules(vec![crate::whitelist::Rule::tool("bash")]);
        let request = build_approval(
            &bash_call("pooprusteek trust --yes"),
            ConversationId::next(),
            false,
        );
        assert!(request.always_ask);
        assert!(
            request.arguments.starts_with("risky:"),
            "{}",
            request.arguments
        );
        // Решение белого списка в приложении и харнессе — `!always_ask && allows`.
        assert!(everything.allows("bash", request.scope.as_ref()));
        assert!(
            request.always_ask,
            "so the whitelist verdict is not consulted"
        );
    }

    #[test]
    fn an_ordinary_command_still_follows_the_whitelist() {
        let request = build_approval(&bash_call("cargo test"), ConversationId::next(), false);
        assert!(!request.always_ask);
        assert!(!request.arguments.starts_with("risky:"));
    }

    /// Родной протокол хранит битые аргументы строкой — такое не исполняется,
    /// и модель узнаёт настоящую причину.
    #[tokio::test]
    async fn arguments_that_are_not_json_are_refused_with_the_reason() {
        let mcp = tokio::sync::Mutex::new(MCPManager::new());
        let outcome = dispatch_generic_tool(
            &ToolRegistry::new(),
            &mcp,
            "read_file",
            serde_json::Value::String("{\"path\": ".to_string()),
        )
        .await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("not valid JSON"), "{}", outcome.text);
        let array = refuse_non_object("edit", &serde_json::json!([1, 2])).expect("refused");
        assert!(
            array.text.contains("must be a JSON object"),
            "{}",
            array.text
        );
    }

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

    #[test]
    fn the_summary_never_shows_a_secret_field() {
        let args = serde_json::json!({"access_token": "tok-123", "server": "main"});
        let summary = tool_call_summary(&args);
        assert_eq!(summary, "main");
        let only_secret = serde_json::json!({"password": "hunter2"});
        assert_eq!(tool_call_summary(&only_secret), "");
    }

    #[test]
    fn the_summary_masks_secrets_inside_a_command() {
        let args = serde_json::json!({"command": "deploy --token s3cr3t && API_KEY=abc run"});
        let summary = tool_call_summary(&args);
        assert!(summary.contains("deploy"), "{summary}");
        assert!(
            !summary.contains("s3cr3t") && !summary.contains("abc"),
            "{summary}"
        );
    }

    #[test]
    fn the_summary_of_a_plain_command_is_unchanged() {
        let args = serde_json::json!({"command": "cargo test items::range"});
        assert_eq!(tool_call_summary(&args), "cargo test items::range");
    }
}
