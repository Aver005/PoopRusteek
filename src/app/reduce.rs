//! Единственное место, где событие хода агента меняет историю беседы.
//! Зовут его все трое — фокусный чат, фоновый и безголовый харнесс.

use crate::app::conversation::Conversation;
use crate::app::events::AgentEvent;

/// Чем кончился ход. Что делать дальше — решает вызывающий: фокусный чат
/// сохраняет сессию и двигает GOAL, фоновый сворачивается в родителя,
/// харнесс закрывает прогон.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Done,
    Failed(String),
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
pub fn turn_tail(end: Option<TurnEnd>, focused: bool, background: bool) -> TurnTail {
    match end {
        None => TurnTail::None,
        Some(TurnEnd::Done) if background => TurnTail::Background(None),
        Some(TurnEnd::Failed(error)) if background => TurnTail::Background(Some(error)),
        Some(_) if !focused => TurnTail::None,
        Some(end) => TurnTail::Focused(end),
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
        | AgentEvent::DiscardEmptyAssistant
        | AgentEvent::ToolStarted { .. }
        | AgentEvent::ToolDone { .. }
        | AgentEvent::ToolError { .. }
        | AgentEvent::ToolOutputCleared { .. }
        | AgentEvent::SessionReset { .. }
        | AgentEvent::ContextUsage(_) => None,
    }
}

/// Применить событие к беседе. Возвращает `Some` только на терминальном
/// событии — по нему вызывающий и ветвится.
pub fn apply(conversation: &mut Conversation, event: &AgentEvent) -> Option<TurnEnd> {
    match event {
        AgentEvent::Started => {
            conversation.generation.begin(std::time::Instant::now());
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
        AgentEvent::DiscardEmptyAssistant => {
            conversation.discard_empty_assistant();
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
            conversation.finish_turn("FINISHED");
            turn_end(event)
        }
        AgentEvent::Failed(_) => {
            conversation.finish_turn("ABORTED");
            turn_end(event)
        }
        // Ход инструмента и сброс серверной сессии историю не трогают:
        // это строка состояния и телеметрия.
        AgentEvent::ToolStarted { .. }
        | AgentEvent::ToolDone { .. }
        | AgentEvent::ToolError { .. }
        | AgentEvent::SessionReset { .. } => None,
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
        apply(&mut conv, &AgentEvent::DiscardEmptyAssistant);
        assert!(conv.messages.is_empty());
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
    fn a_side_chat_folds_into_its_parent_whether_or_not_it_is_on_screen() {
        for focused in [true, false] {
            assert_eq!(
                turn_tail(Some(TurnEnd::Done), focused, true),
                TurnTail::Background(None)
            );
            assert_eq!(
                turn_tail(Some(TurnEnd::Failed("нет".to_string())), focused, true),
                TurnTail::Background(Some("нет".to_string()))
            );
        }
    }

    #[test]
    fn a_parallel_chat_off_screen_gets_no_tail() {
        // Статистика, автосохранение и GOAL относятся к тому, что на экране:
        // прогон соседнего чата не должен их дёргать.
        assert_eq!(turn_tail(Some(TurnEnd::Done), false, false), TurnTail::None);
        assert_eq!(
            turn_tail(Some(TurnEnd::Failed("нет".to_string())), false, false),
            TurnTail::None
        );
    }

    #[test]
    fn the_chat_on_screen_keeps_its_own_tail() {
        assert_eq!(
            turn_tail(Some(TurnEnd::Done), true, false),
            TurnTail::Focused(TurnEnd::Done)
        );
    }

    #[test]
    fn a_turn_that_has_not_ended_has_no_tail() {
        for focused in [true, false] {
            for background in [true, false] {
                assert_eq!(turn_tail(None, focused, background), TurnTail::None);
            }
        }
    }

    #[test]
    fn context_usage_lands_on_the_conversation() {
        let mut conv = conversation();
        apply(&mut conv, &AgentEvent::ContextUsage(1234));
        assert_eq!(conv.context_used, 1234);
    }
}
