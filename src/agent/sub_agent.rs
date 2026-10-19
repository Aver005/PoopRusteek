//! Суб-агент `task`: тот же `run_agent_loop`, что и у основного хода, на
//! форкнутом провайдере, в своём канале событий и внутри future родителя.

use crate::agent::tools_step::{ToolExecContext, ToolOutcome};
use crate::app::conversation::ConversationId;
use crate::app::events::{AgentEvent, AgentResult, AppEvent};
use crate::app::runtime::TurnSpec;
use crate::provider::ChatMessage;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Сколько шагов у суб-агента и фонового хода. Модель узнаёт об этом из
/// [`unattended_note`].
pub const SUB_AGENT_MAX_STEPS: usize = 16;

/// Сколько последней реплики суб-агента показать родителю при провале.
const LAST_MESSAGE_BYTES: usize = 600;

/// Что узнаёт ход без человека (суб-агент, фоновый чат): без этого он тратил
/// шаги на `task`/`question`/`timer`, которые ему всегда отказывают.
pub fn unattended_note(max_steps: usize) -> String {
    format!(
        "You are running unattended: as a sub-agent for another agent or as a background task. You have at most {max_steps} steps. The tools task, question and timer are not available to you. End with a final answer that states what you did and what you found — that answer is all that will be reported back."
    )
}

/// Запустить суб-агента и дождаться итога. Отмена родителя роняет этот
/// future, а с ним цикл суб-агента, его поток и его команды. Тип стёрт:
/// рекурсия `run_agent_loop` → `task` иначе бесконечна, и `Send` не доказать.
pub(super) fn run_task<'a>(
    ctx: &'a ToolExecContext<'_>,
    label: &'a str,
    prompt: String,
) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
    Box::pin(run(ctx, label, prompt))
}

async fn run(ctx: &ToolExecContext<'_>, label: &str, prompt: String) -> ToolOutcome {
    let conversation = ConversationId::next();
    let provider = ctx.provider.fork();
    // Форк одноразовый: его серверная сессия удаляется и при отмене.
    let discard = crate::provider::SessionDiscard::new(Arc::clone(&provider));
    let max_steps = ctx.max_steps.min(SUB_AGENT_MAX_STEPS);
    crate::debug_log::log_json(
        "agent.sub_agent.started",
        &serde_json::json!({
            "parent": ctx.conversation.0,
            "conversation": conversation.0,
            "label": label,
            "prompt": prompt,
            "max_steps": max_steps,
        }),
    );

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let spec = TurnSpec {
        conversation,
        provider,
        messages: vec![ChatMessage::user(&prompt)],
        system_prompt: format!("{}\n\n{}", ctx.system_prompt, unattended_note(max_steps)),
        model: ctx.model.to_string(),
        temperature: ctx.temperature,
        max_tokens: ctx.max_tokens,
        max_steps,
        max_tools_per_step: ctx.max_tools_per_step,
        unattended: true,
        tool_output_limit: ctx.tool_output_limit,
        context: ctx.context.clone(),
    };
    let turn = crate::app::runtime::run_guarded(
        spec,
        Arc::clone(ctx.tools),
        Arc::clone(ctx.mcp),
        Arc::clone(ctx.semantic),
        event_tx,
    );
    let ((), run) = tokio::join!(turn, relay(ctx, label, event_rx));
    discard.now().await;
    let outcome = run.outcome(label);
    crate::debug_log::log_json(
        "agent.sub_agent.finished",
        &serde_json::json!({
            "conversation": conversation.0,
            "is_error": outcome.is_error,
            "result": outcome.text,
        }),
    );
    outcome
}

/// Что видно о суб-агенте, пока он работает.
#[derive(Default)]
struct Run {
    end: Option<Result<AgentResult, String>>,
    /// Последняя непустая реплика: при провале родитель видит, где тот остановился.
    last_text: String,
    /// Инструменты, которые правда запускались (отказанные сюда не попадают).
    started: Vec<String>,
}

impl Run {
    fn outcome(self, label: &str) -> ToolOutcome {
        let report = report(
            label,
            self.end.as_ref().map(|end| {
                end.as_ref()
                    .map(|result| result.text.as_str())
                    .map_err(String::as_str)
            }),
            &self.last_text,
            &self.started,
        );
        let truncated = self
            .end
            .as_ref()
            .and_then(|end| end.as_ref().ok())
            .and_then(|result| result.truncated.as_deref());
        match (&self.end, truncated) {
            (Some(Ok(_)), None) => ToolOutcome::ok(report),
            // Обрезанный лимитом ответ — не чистый успех.
            (Some(Ok(_)), Some(note)) => ToolOutcome::ok(format!(
                "{report}\n[the sub-agent's answer is incomplete: {note}]"
            )),
            _ => ToolOutcome::failed(report),
        }
    }
}

/// Итог суб-агента для того, кто его запускал, — общий для `task` и фоновых.
/// `None` — итога нет: упал без ответа.
pub fn report(
    label: &str,
    end: Option<Result<&str, &str>>,
    last_text: &str,
    started: &[String],
) -> String {
    let work = work_summary(started);
    let last_words = if last_text.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nIts last message: «{}»",
            crate::util::truncate_with_ellipsis(last_text.trim(), LAST_MESSAGE_BYTES)
        )
    };
    match end {
        Some(Ok(text)) => format!("{text}{work}"),
        Some(Err(reason)) => format!("Sub-agent '{label}' failed: {reason}{last_words}"),
        None => format!("Sub-agent '{label}' ended without a result.{work}{last_words}"),
    }
}

/// Итог фонового хода, каким его видит родительский чат — в TUI и в `exec`.
pub fn background_block(label: &str, title: &str, report: &str) -> String {
    format!("{label} — {title}\n\n{report}")
}

/// Сводка сделанного суб-агентом — одной строкой в конце его ответа.
fn work_summary(started: &[String]) -> String {
    let mut distinct: Vec<&str> = Vec::new();
    for name in started {
        if !distinct.contains(&name.as_str()) {
            distinct.push(name);
        }
    }
    if started.is_empty() {
        return String::new();
    }
    format!(
        "\n\n[sub-agent ran {} tool call(s): {}]",
        started.len(),
        distinct.join(", ")
    )
}

/// Пересылать родителю то, что требует человека или видно в строке
/// состояния, и копить итог суб-агента.
async fn relay(
    ctx: &ToolExecContext<'_>,
    label: &str,
    mut events: mpsc::UnboundedReceiver<AppEvent>,
) -> Run {
    let mut run = Run::default();
    // Конец вызова пересылается, только если пересылалось его начало: иначе
    // отказ без запуска закрыл бы у родителя сам вызов `task`.
    let mut tool_running = false;
    while let Some(event) = events.recv().await {
        match event {
            AppEvent::Agent { event, .. } => match event {
                AgentEvent::ToolStarted { name } => {
                    ctx.emit(AgentEvent::ToolStarted {
                        name: format!("{label} › {name}"),
                    });
                    run.started.push(name);
                    tool_running = true;
                }
                AgentEvent::ToolDone { result } if tool_running => {
                    tool_running = false;
                    ctx.emit(AgentEvent::ToolDone { result });
                }
                AgentEvent::ToolError { error } if tool_running => {
                    tool_running = false;
                    ctx.emit(AgentEvent::ToolError { error });
                }
                AgentEvent::EndAssistantMessage { text, .. } if !text.trim().is_empty() => {
                    run.last_text = text;
                }
                AgentEvent::Done(result) => run.end = Some(Ok(result)),
                AgentEvent::Failed(reason) => run.end = Some(Err(reason)),
                _ => {}
            },
            // Спрашивает суб-агент, а отвечает за него тот, кто отвечает за
            // родителя. Отмена родителя снимает и этот запрос.
            AppEvent::RequestToolApproval(mut request) => {
                request.conversation = ctx.conversation;
                request.origin = Some(format!("sub-agent «{label}»"));
                let forwarded = request.clone();
                if ctx
                    .event_tx
                    .send(AppEvent::RequestToolApproval(forwarded))
                    .is_err()
                {
                    // Отвечать некому — отказ, а не вечное ожидание.
                    request.resolve(false).await;
                }
            }
            // Ход без человека не спрашивает: дошёл вопрос — это регрессия, и
            // ответ «отменено» лучше, чем суб-агент, ждущий вечно.
            AppEvent::RequestQuestion(request, _) => {
                crate::debug_log::log("agent.sub_agent.unexpected_question", label);
                request.resolve(String::new()).await;
            }
            _ => {}
        }
    }
    // Суб-агент упал посреди инструмента: закрыть его у родителя, иначе конец
    // самого `task` закрыл бы чужой вызов.
    if tool_running {
        ctx.emit(AgentEvent::ToolError {
            error: "the sub-agent stopped before this tool finished".to_string(),
        });
    }
    run
}

#[cfg(test)]
mod tests {
    use super::{report, work_summary};
    use crate::agent::runner::run_agent_loop;
    use crate::app::conversation::ConversationId;
    use crate::app::events::{AgentEvent, AppEvent};
    use crate::app::runtime::TurnSpec;
    use crate::mcp::MCPManager;
    use crate::provider::{ChatMessage, LLMProvider, Role, fake::FakeProvider};
    use crate::semantic::SemanticService;
    use crate::tools::registry::ToolRegistry;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    const TASK: &str = "<tool_use><name>task</name><arguments>{\"description\": \"look\", \"prompt\": \"find it\"}</arguments></tool_use>";
    const LOUD: &str = "<tool_use><name>loud</name><arguments>{}</arguments></tool_use>";
    const ASK: &str = "<tool_use><name>question</name><arguments>{\"question\": \"which?\", \"type\": \"yes_no\"}</arguments></tool_use>";

    /// Инструмент с подтверждением — его вызов суб-агентом обязан дойти до
    /// того, кто подтверждает за родителя.
    struct Loud;

    #[async_trait::async_trait]
    impl crate::tools::Tool for Loud {
        fn definition(&self) -> crate::tools::ToolDefinition {
            crate::tools::ToolDefinition {
                name: "loud".to_string(),
                description: "needs approval".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        async fn execute(&self, _args: serde_json::Value) -> crate::tools::ToolResult {
            crate::tools::ToolResult {
                content: "loud ran".to_string(),
                is_error: false,
            }
        }
    }

    struct Turn {
        /// Результат `task`, каким его увидел родитель.
        task_result: ChatMessage,
        /// Кто спросил и откуда, по каждому запросу подтверждения.
        approvals: Vec<(ConversationId, Option<String>)>,
        parent: ConversationId,
    }

    /// Родитель зовёт `task`, суб-агент отвечает по `sub`; на подтверждения
    /// «человек» отвечает `approve` — как в TUI или в `exec`.
    async fn parent_turn(sub: Vec<String>, max_steps: usize, approve: bool) -> Turn {
        let parent = Arc::new(
            FakeProvider::with_responses(vec![TASK.to_string(), "Parent done.".to_string()])
                .forks_with(sub),
        );
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(Loud));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let conversation = ConversationId::next();
        let turn = tokio::spawn(run_agent_loop(
            TurnSpec {
                conversation,
                provider: Arc::clone(&parent) as Arc<dyn LLMProvider>,
                messages: vec![ChatMessage::user("go")],
                system_prompt: "system".to_string(),
                model: "fake".to_string(),
                temperature: 0.0,
                max_tokens: 128,
                max_steps,
                max_tools_per_step: 4,
                unattended: false,
                tool_output_limit: 0,
                context: crate::context::ContextSpec::default(),
            },
            tools,
            Arc::new(tokio::sync::Mutex::new(MCPManager::new())),
            SemanticService::disabled(),
            event_tx,
        ));

        let mut approvals = Vec::new();
        let mut task_result = None;
        let wait = async {
            while let Some(event) = event_rx.recv().await {
                match event {
                    AppEvent::RequestToolApproval(request) => {
                        approvals.push((request.conversation, request.origin.clone()));
                        request.resolve(approve).await;
                    }
                    AppEvent::Agent {
                        event: AgentEvent::Message(message),
                        ..
                    } if message.role == Role::Tool && message.name.as_deref() == Some("task") => {
                        task_result = Some(message);
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), wait)
            .await
            .expect("the parent turn must finish");
        turn.await.unwrap();
        Turn {
            task_result: task_result.expect("the parent got a task result"),
            approvals,
            parent: conversation,
        }
    }

    #[tokio::test]
    async fn a_sub_agent_answers_with_what_it_did() {
        let turn = parent_turn(vec![LOUD.to_string(), "Found it.".to_string()], 8, true).await;
        let result = &turn.task_result;
        assert!(!result.tool_error, "{}", result.content);
        assert!(
            result.content.starts_with("Found it."),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("ran 1 tool call(s): loud"),
            "{}",
            result.content
        );
        // Раньше суб-агент исполнял всё без спроса: один `task` обходил отказ.
        let (asker, origin) = turn.approvals.first().expect("loud needs approval");
        // От имени родителя: отмена его хода снимает и этот запрос.
        assert_eq!(*asker, turn.parent);
        assert_eq!(origin.as_deref(), Some("sub-agent «look»"));
    }

    /// Отказ человека суб-агенту — настоящий отказ, и в сводку «что сделано»
    /// неисполненный вызов не попадает.
    #[tokio::test]
    async fn a_denied_sub_agent_call_is_not_reported_as_run() {
        let turn = parent_turn(vec![LOUD.to_string(), "Could not.".to_string()], 8, false).await;
        let result = &turn.task_result;
        assert!(!turn.approvals.is_empty());
        assert!(
            result.content.starts_with("Could not."),
            "{}",
            result.content
        );
        assert!(
            !result.content.contains("ran 1 tool call"),
            "{}",
            result.content
        );
    }

    /// Ход без человека не спрашивает — вопрос отклонён, а не повис.
    #[tokio::test]
    async fn a_sub_agent_cannot_ask_the_user() {
        let turn = parent_turn(vec![ASK.to_string(), "Gave up.".to_string()], 8, true).await;
        assert!(turn.task_result.content.starts_with("Gave up."));
        assert!(turn.approvals.is_empty());
    }

    /// Шаги кончились — это провал с последней репликой, а не «успешный» ответ
    /// фразой на полпути.
    #[tokio::test]
    async fn a_sub_agent_out_of_steps_is_a_failure_with_its_last_words() {
        let narrated = format!("Looking around.\n{LOUD}");
        let turn = parent_turn(vec![narrated.clone(), narrated], 2, true).await;
        let result = &turn.task_result;
        assert!(result.tool_error, "{}", result.content);
        assert!(
            result.content.contains("limit of 2 agent steps"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("Looking around."),
            "{}",
            result.content
        );
    }

    #[test]
    fn the_work_summary_counts_runs_and_names_each_tool_once() {
        assert_eq!(work_summary(&[]), "");
        let summary = work_summary(&["bash".into(), "edit".into(), "bash".into()]);
        assert_eq!(summary, "\n\n[sub-agent ran 3 tool call(s): bash, edit]");
    }

    #[test]
    fn a_report_without_a_result_names_what_ran() {
        let text = report("look", None, "half way", &["bash".into()]);
        assert!(text.contains("ended without a result"), "{text}");
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("half way"), "{text}");
    }
}
