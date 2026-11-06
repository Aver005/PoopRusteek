//! Единственное место, где событие хода агента меняет историю беседы.
//! Зовут его все трое — фокусный чат, фоновый и безголовый харнесс.

use crate::app::conversation::{Conversation, ToolRun};
use crate::app::events::AgentEvent;

/// Чем кончился ход. Что делать дальше — решает вызывающий: фокусный чат
/// сохраняет сессию и двигает GOAL, фоновый сворачивается в родителя,
/// харнесс закрывает прогон.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Done,
    Failed(String),
}

/// Чей это ход. Два `bool` подряд в сигнатуре переставлялись местами молча —
/// здесь развилка названа и разобрана один раз.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOwner {
    /// Чат на экране.
    Focused,
    /// Побочный чат (`/btw`, суб-агент).
    Background,
    /// Параллельный чат не на экране.
    Parallel,
}

impl TurnOwner {
    /// Побочный чат сворачивается в родителя независимо от того, смотрят на
    /// него или нет, — поэтому он проверяется первым.
    pub fn classify(focused: bool, background: bool) -> Self {
        match (background, focused) {
            (true, _) => Self::Background,
            (false, true) => Self::Focused,
            (false, false) => Self::Parallel,
        }
    }
}

/// Кому принадлежит хвост завершившегося хода. Три случая легко перепутать,
/// поэтому развилка вынесена отдельно и покрыта тестами.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnTail {
    /// Побочный чат (`/btw`, суб-агент) сворачивается в родителя.
    Background(Option<String>),
    /// Чат на экране: статистика, автосохранение, шаг GOAL.
    Focused(TurnEnd),
    /// Ход не завершился, либо это параллельный чат не на экране: историю он
    /// обновил, а хвост — не его.
    None,
}

/// Разложить исход хода по тому, чей он.
pub fn turn_tail(end: Option<TurnEnd>, owner: TurnOwner) -> TurnTail {
    match (end, owner) {
        (None, _) | (Some(_), TurnOwner::Parallel) => TurnTail::None,
        (Some(TurnEnd::Done), TurnOwner::Background) => TurnTail::Background(None),
        (Some(TurnEnd::Failed(error)), TurnOwner::Background) => TurnTail::Background(Some(error)),
        (Some(end), TurnOwner::Focused) => TurnTail::Focused(end),
    }
}

/// Чем кончился ход, без изменения истории. Для потребителя, которому эта
/// беседа не принадлежит, — он ведёт только учёт незакрытых ходов.
pub fn turn_end(event: &AgentEvent) -> Option<TurnEnd> {
    match event {
        AgentEvent::Done(_) => Some(TurnEnd::Done),
        AgentEvent::Failed(error) => Some(TurnEnd::Failed(error.clone())),
        // Перечислено поимённо, а не `_`: новое терминальное событие обязано
        // сломать сборку здесь, а не тихо перестать закрывать ход.
        AgentEvent::Started
        | AgentEvent::BeginAssistantMessage
        | AgentEvent::Chunk(_)
        | AgentEvent::Message(_)
        | AgentEvent::EndAssistantMessage { .. }
        | AgentEvent::ToolStarted { .. }
        | AgentEvent::ToolDone { .. }
        | AgentEvent::ToolError { .. }
        | AgentEvent::ToolOutputCleared { .. }
        | AgentEvent::SessionReset { .. }
        | AgentEvent::ContextUsage(_) => None,
    }
}

/// Сколько причины провала попадает в историю: тело ответа WAF или битый
/// вызов модели иначе ехали бы в каждый следующий запрос целиком.
const FAILURE_NOTE_REASON_BYTES: usize = 900;

/// Заметка о провале хода — и человеку в ленте, и модели в следующем запросе.
/// Причина в кавычках: это отчёт рантайма, а не указание.
pub fn failure_note(reason: &str) -> String {
    let reason = crate::util::truncate_with_ellipsis(reason.trim(), FAILURE_NOTE_REASON_BYTES);
    format!(
        "⚠ An earlier turn in this conversation failed and was stopped by the agent runtime. Reported reason: «{reason}». A reply right before this note may be incomplete."
    )
}

/// Применить событие к беседе. Возвращает `Some` только на терминальном
/// событии — по нему вызывающий и ветвится.
pub fn apply(conversation: &mut Conversation, event: &AgentEvent) -> Option<TurnEnd> {
    match event {
        AgentEvent::Started => {
            conversation.generation.begin(std::time::Instant::now());
            conversation.current_tool = None;
            None
        }
        AgentEvent::BeginAssistantMessage => {
            conversation.begin_assistant_message();
            None
        }
        AgentEvent::Chunk(text) => {
            conversation.append_chunk(text);
            None
        }
        AgentEvent::Message(message) => {
            conversation.messages.push(message.clone());
            None
        }
        AgentEvent::EndAssistantMessage { tool_calls, text } => {
            conversation.end_assistant_message(tool_calls, text);
            None
        }
        AgentEvent::ContextUsage(used) => {
            conversation.context_used = *used;
            None
        }
        AgentEvent::ToolOutputCleared { cleared, .. } => {
            conversation.clear_tool_output(cleared);
            None
        }
        AgentEvent::Done(_) => {
            conversation.current_tool = None;
            conversation.finish_turn("FINISHED");
            conversation.flush_deferred();
            turn_end(event)
        }
        AgentEvent::Failed(reason) => {
            conversation.current_tool = None;
            conversation.finish_turn("ABORTED");
            // Причина остаётся в истории: строка состояния её забывала, а модель
            // следующего хода не знала, что прошлый оборвался.
            conversation
                .messages
                .push(crate::provider::ChatMessage::system(&failure_note(reason)));
            conversation.flush_deferred();
            turn_end(event)
        }
        // Инструмент историю не трогает (его результат придёт сообщением):
        // это только «что делается сейчас» для панели работы.
        AgentEvent::ToolStarted { name, summary } => {
            conversation.current_tool = Some(ToolRun {
                name: name.clone(),
                summary: summary.clone(),
                started: std::time::Instant::now(),
            });
            None
        }
        AgentEvent::ToolDone { .. } | AgentEvent::ToolError { .. } => {
            conversation.current_tool = None;
            None
        }
        // Сброс серверной сессии историю не трогает: строка состояния и телеметрия.
        AgentEvent::SessionReset { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::events::AgentResult;
    use crate::provider::{ChatMessage, Role};

    fn conversation() -> Conversation {
        Conversation::fresh_main(None)
    }

    #[test]
    fn a_stream_builds_one_assistant_message() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::BeginAssistantMessage);
        apply(&mut conv, &AgentEvent::Chunk("при".to_string()));
        apply(&mut conv, &AgentEvent::Chunk("вет".to_string()));
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, Role::Assistant);
        assert_eq!(conv.messages[0].content, "привет");
    }

    #[test]
    fn an_assistant_message_that_never_got_text_is_dropped() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::BeginAssistantMessage);
        apply(
            &mut conv,
            &AgentEvent::EndAssistantMessage {
                tool_calls: Vec::new(),
                text: String::new(),
            },
        );
        assert!(conv.messages.is_empty());
    }

    /// Поток обрезан на первом `<`; итог шага возвращает прозу после вызова.
    #[test]
    fn the_step_result_replaces_what_the_stream_showed() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::BeginAssistantMessage);
        apply(&mut conv, &AgentEvent::Chunk("Reading.\n".to_string()));
        apply(
            &mut conv,
            &AgentEvent::EndAssistantMessage {
                tool_calls: Vec::new(),
                text: "Reading.\n\nThen I'll edit it.".to_string(),
            },
        );
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Reading.\n\nThen I'll edit it.");
    }

    #[test]
    fn a_finished_turn_reports_it_and_clears_the_empty_tail() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::BeginAssistantMessage);
        let end = apply(
            &mut conv,
            &AgentEvent::Done(AgentResult {
                text: String::new(),
                tool_calls: Vec::new(),
                truncated: None,
            }),
        );
        assert_eq!(end, Some(TurnEnd::Done));
        assert!(conv.messages.is_empty());
        assert!(!conv.generation.active);
    }

    #[test]
    fn a_failed_turn_carries_its_reason_out() {
        let mut conv = conversation();
        let end = apply(
            &mut conv,
            &AgentEvent::Failed("сеть отвалилась".to_string()),
        );
        assert_eq!(end, Some(TurnEnd::Failed("сеть отвалилась".to_string())));
        assert!(!conv.generation.active);
    }

    /// Причина провала не только в строке состояния: следующий ход её увидит.
    #[test]
    fn a_failed_turn_leaves_its_reason_in_the_history() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::BeginAssistantMessage);
        apply(&mut conv, &AgentEvent::Chunk("Начинаю пра".to_string()));
        apply(
            &mut conv,
            &AgentEvent::Failed("сеть отвалилась".to_string()),
        );
        let last = conv.messages.last().expect("заметка о провале");
        assert_eq!(last.role, Role::System);
        assert!(!last.ui_only, "заметка должна дойти до модели");
        assert!(last.content.contains("сеть отвалилась"), "{}", last.content);
        assert!(last.content.contains("incomplete"), "{}", last.content);
    }

    /// Итог фонового суб-агента посреди хода раньше вставал внутрь хода, и
    /// DeepSeek (шлёт хвост после последнего ответа) его не видел никогда.
    #[test]
    fn a_note_arriving_mid_turn_lands_after_the_turn() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::Started);
        apply(&mut conv, &AgentEvent::BeginAssistantMessage);
        conv.deliver_note(ChatMessage::system("sub-agent result"));
        apply(&mut conv, &AgentEvent::Chunk("answer".to_string()));
        assert!(
            conv.messages
                .iter()
                .all(|m| m.content != "sub-agent result")
        );
        apply(
            &mut conv,
            &AgentEvent::Done(AgentResult {
                text: "answer".to_string(),
                tool_calls: Vec::new(),
                truncated: None,
            }),
        );
        let last = conv.messages.last().expect("the note");
        assert_eq!(last.content, "sub-agent result");
        assert_eq!(conv.messages[conv.messages.len() - 2].content, "answer");
    }

    /// Тело ответа WAF не должно ехать в каждый следующий запрос целиком.
    #[test]
    fn a_huge_failure_reason_is_capped() {
        let note = failure_note(&"<html>".repeat(10_000));
        assert!(note.len() < 2_000, "{} bytes", note.len());
    }

    #[test]
    fn cleared_tool_output_is_matched_by_id_not_by_index() {
        let mut conv = conversation();
        conv.messages.push(ChatMessage::user("вопрос"));
        // `ui_only` есть здесь и нет в копии цикла агента: правка по индексу
        // легла бы на соседнее сообщение.
        conv.messages.push(ChatMessage {
            ui_only: true,
            ..ChatMessage::assistant("(строка состояния)")
        });
        conv.messages
            .push(ChatMessage::tool("call-b", "длинный вывод"));
        conv.messages
            .push(ChatMessage::tool("call-a", "другой вывод"));

        apply(
            &mut conv,
            &AgentEvent::ToolOutputCleared {
                cleared: vec![("call-a".to_string(), "[очищено]".to_string())],
                freed_tokens: 42,
            },
        );

        assert_eq!(conv.messages[1].content, "(строка состояния)");
        assert_eq!(conv.messages[2].content, "длинный вывод");
        assert_eq!(conv.messages[3].content, "[очищено]");
    }

    #[test]
    fn an_unknown_tool_call_id_clears_nothing() {
        let mut conv = conversation();
        conv.messages.push(ChatMessage::tool("call-a", "вывод"));
        apply(
            &mut conv,
            &AgentEvent::ToolOutputCleared {
                cleared: vec![("call-z".to_string(), "[очищено]".to_string())],
                freed_tokens: 0,
            },
        );
        assert_eq!(conv.messages[0].content, "вывод");
    }

    #[test]
    fn status_only_events_leave_the_history_alone() {
        let mut conv = conversation();
        conv.messages.push(ChatMessage::user("вопрос"));
        for event in [
            AgentEvent::ToolStarted {
                name: "bash".to_string(),
                summary: "cargo test".to_string(),
            },
            AgentEvent::ToolDone {
                result: "ок".to_string(),
            },
            AgentEvent::ToolError {
                error: "нет".to_string(),
            },
            AgentEvent::SessionReset {
                before_tokens: 100,
                after_tokens: 10,
            },
        ] {
            assert_eq!(apply(&mut conv, &event), None);
        }
        assert_eq!(conv.messages.len(), 1);
    }

    #[test]
    fn a_side_chat_is_background_whether_or_not_it_is_on_screen() {
        for focused in [true, false] {
            assert_eq!(
                TurnOwner::classify(focused, true),
                TurnOwner::Background,
                "focused={focused}"
            );
        }
        assert_eq!(TurnOwner::classify(true, false), TurnOwner::Focused);
        assert_eq!(TurnOwner::classify(false, false), TurnOwner::Parallel);
    }

    #[test]
    fn a_side_chat_folds_into_its_parent() {
        assert_eq!(
            turn_tail(Some(TurnEnd::Done), TurnOwner::Background),
            TurnTail::Background(None)
        );
        assert_eq!(
            turn_tail(
                Some(TurnEnd::Failed("нет".to_string())),
                TurnOwner::Background
            ),
            TurnTail::Background(Some("нет".to_string()))
        );
    }

    #[test]
    fn a_parallel_chat_off_screen_gets_no_tail() {
        // Статистика, автосохранение и GOAL относятся к тому, что на экране:
        // прогон соседнего чата не должен их дёргать.
        assert_eq!(
            turn_tail(Some(TurnEnd::Done), TurnOwner::Parallel),
            TurnTail::None
        );
        assert_eq!(
            turn_tail(
                Some(TurnEnd::Failed("нет".to_string())),
                TurnOwner::Parallel
            ),
            TurnTail::None
        );
    }

    #[test]
    fn the_chat_on_screen_keeps_its_own_tail() {
        assert_eq!(
            turn_tail(Some(TurnEnd::Done), TurnOwner::Focused),
            TurnTail::Focused(TurnEnd::Done)
        );
    }

    #[test]
    fn a_turn_that_has_not_ended_has_no_tail() {
        for owner in [
            TurnOwner::Focused,
            TurnOwner::Background,
            TurnOwner::Parallel,
        ] {
            assert_eq!(turn_tail(None, owner), TurnTail::None, "{owner:?}");
        }
    }

    #[test]
    fn context_usage_lands_on_the_conversation() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::ContextUsage(1234));
        assert_eq!(conv.context_used, 1234);
    }

    #[test]
    fn a_running_tool_is_tracked_until_it_ends() {
        let mut conv = conversation();
        apply(
            &mut conv,
            &AgentEvent::ToolStarted {
                name: "bash".to_string(),
                summary: "cargo test".to_string(),
            },
        );
        let run = conv.current_tool.as_ref().expect("tool is running");
        assert_eq!(
            (run.name.as_str(), run.summary.as_str()),
            ("bash", "cargo test")
        );
        apply(
            &mut conv,
            &AgentEvent::ToolDone {
                result: "ok".to_string(),
            },
        );
        assert!(conv.current_tool.is_none());
    }

    #[test]
    fn a_failed_tool_and_a_finished_turn_both_clear_the_running_tool() {
        for ending in [
            AgentEvent::ToolError {
                error: "нет".to_string(),
            },
            AgentEvent::Failed("сеть".to_string()),
            AgentEvent::Done(AgentResult {
                text: String::new(),
                tool_calls: Vec::new(),
                truncated: None,
            }),
        ] {
            let mut conv = conversation();
            apply(
                &mut conv,
                &AgentEvent::ToolStarted {
                    name: "grep".to_string(),
                    summary: String::new(),
                },
            );
            apply(&mut conv, &ending);
            assert!(conv.current_tool.is_none(), "{ending:?}");
        }
    }
}
