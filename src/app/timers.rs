//! Срабатывание отложенных задач.
//!
//! Взводит их вызов `timer` (`agent::tools_step`), хранит
//! `tools::timer::TimerStore`, а будит — тик главного цикла: только `App`
//! знает, жива ли беседа-владелец и не идёт ли в ней ход.

use super::App;
use super::conversation::ConversationId;
use crate::error::AppResult;
use crate::provider::ChatMessage;
use crate::tools::timer::Timer;
use chrono::{Duration, Utc};

/// Занятую беседу переспрашиваем через этот шаг…
const DEFER_STEP_SECS: i64 = 5;
/// …но не дольше пяти минут: дальше побудка вырождается в напоминание.
const MAX_DEFERRALS: u32 = 60;

/// Куда девать сработавший таймер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Поднять ход в беседе-владельце.
    Wake,
    /// Показать напоминание, ход не поднимать.
    Notify,
    /// В беседе идёт ход — попробовать позже.
    Defer,
    /// Беседы-владельца больше нет.
    Orphan,
}

/// Чистое правило доставки: развилку легко перепутать, поэтому она одна и
/// покрыта тестами. Бюджет побудок здесь не считается — его занимает уже
/// сама доставка, иначе отложенный таймер съедал бы слот впустую.
pub(crate) fn route(timer: &Timer, owner_alive: bool, owner_busy: bool) -> Delivery {
    match (owner_alive, timer.wake, owner_busy) {
        (false, _, _) => Delivery::Orphan,
        (true, false, _) => Delivery::Notify,
        (true, true, true) if timer.deferrals < MAX_DEFERRALS => Delivery::Defer,
        (true, true, true) => Delivery::Notify,
        (true, true, false) => Delivery::Wake,
    }
}

/// Сообщение, поднимающее ход. Роль — пользовательская: у DeepSeek хвост
/// с `system` рендерится как заметка, а действовать надо по вводу.
fn wake_message(timer: &Timer) -> ChatMessage {
    ChatMessage::user_with_display(
        &format!(
            "⏰ Timer #{} fired — this is an automatic timer, not a message from the user.\n\
            Note: {}\n\
            Act on it now; if it needs the user, say so in your answer.",
            timer.id, timer.note
        ),
        &format!("⏰ timer #{}: {}", timer.id, timer.note),
    )
}

/// Текст напоминания. `throttled` — побудка выродилась в уведомление:
/// беседа занята слишком долго или бюджет побудок исчерпан.
fn notice_text(timer: &Timer, throttled: bool) -> String {
    let mut text = format!("⏰ Timer #{}: {}", timer.id, timer.note);
    if throttled {
        text.push_str("\n(the chat was busy or already resumed by timers — not resumed)");
    }
    text
}

impl App {
    /// Забрать созревшие таймеры и доставить каждый. Зовут с тика, поэтому
    /// пустой случай стоит один короткий лок (инвариант 1).
    pub(crate) async fn fire_due_timers(&mut self) -> AppResult<()> {
        let timers = self.tools.timers();
        let now = Utc::now();
        for timer in timers.take_due(now) {
            let owner = ConversationId(timer.owner);
            let target = self.state.conversations.get(owner);
            let alive = target.is_some();
            let busy = target
                .map(|c| c.generation.active || c.compacting.is_some())
                .unwrap_or(false);

            match route(&timer, alive, busy) {
                Delivery::Defer => timers.defer(timer, now + Duration::seconds(DEFER_STEP_SECS)),
                Delivery::Orphan => {
                    let text = format!(
                        "⏰ Timer #{} fired, but its chat is gone: {}",
                        timer.id, timer.note
                    );
                    self.state
                        .focused_mut()
                        .messages
                        .push(ChatMessage::ui_system(&text));
                    self.state.status_message = format!("⏰ Timer #{} — chat gone", timer.id);
                }
                Delivery::Notify => self.deliver_timer_notice(owner, &timer, false),
                Delivery::Wake => {
                    if timers.take_wake_slot(timer.owner) {
                        self.state.status_message = format!("⏰ Timer #{} — resuming", timer.id);
                        self.send_turn(owner, Some(wake_message(&timer))).await?;
                    } else {
                        self.deliver_timer_notice(owner, &timer, true);
                    }
                }
            }
        }
        self.state.timers_pending = timers.pending();
        Ok(())
    }

    /// Напоминание уходит в беседу-владельца; если человек смотрит в другую,
    /// туда идёт короткая строка — так же сворачивается фоновый суб-агент.
    fn deliver_timer_notice(&mut self, owner: ConversationId, timer: &Timer, throttled: bool) {
        let focused = self.state.conversations.focused_id();
        if let Some(conv) = self.state.conversations.get_mut(owner) {
            conv.messages
                .push(ChatMessage::ui_system(&notice_text(timer, throttled)));
        }
        if owner != focused {
            self.state
                .focused_mut()
                .messages
                .push(ChatMessage::ui_system(&format!(
                    "⏰ Timer #{} fired in another chat — {}",
                    timer.id, timer.note
                )));
        }
        self.state.status_message = format!("⏰ Timer #{} fired", timer.id);
    }

    /// Снять таймеры закрытой беседы: иначе они выстрелят в пустоту.
    pub(crate) fn drop_timers_of(&mut self, owner: ConversationId) {
        self.tools.timers().cancel_owner(owner.0);
    }

    /// Человек написал сам — цепочка автоматических побудок оборвана.
    pub(crate) fn reset_timer_wakes(&mut self, owner: ConversationId) {
        self.tools.timers().reset_wakes(owner.0);
    }

    /// Строки для `/timers`.
    pub(crate) fn build_timers_display(&self) -> String {
        let now = Utc::now();
        let focused = self.state.conversations.focused_id();
        let pending = self.tools.timers().list(None);
        if pending.is_empty() {
            return "No timers pending. The agent sets them with the `timer` tool; they are lost on exit.".to_string();
        }
        let mut lines = vec![format!("⏰ {} timer(s):", pending.len())];
        for timer in pending {
            let here = if timer.owner == focused.0 {
                ""
            } else {
                " (another chat)"
            };
            lines.push(format!("  {}{here}", timer.describe(now)));
        }
        lines.push("Cancel one with /timers cancel <id>.".to_string());
        lines.join("\n")
    }

    /// `/timers cancel <id>` — отменяет в любой беседе: человек видит весь
    /// список, ограничивать его своей нечем.
    pub(crate) fn cancel_timer_by_id(&mut self, id: u64) -> String {
        match self.tools.timers().cancel(id, None) {
            Some(timer) => format!("Cancelled timer #{}: {}", id, timer.note),
            None => format!("No timer #{id}."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(wake: bool, deferrals: u32) -> Timer {
        Timer {
            id: 1,
            owner: 7,
            due_at: Utc::now(),
            note: "check the build".to_string(),
            wake,
            deferrals,
        }
    }

    #[test]
    fn a_dead_chat_never_gets_resurrected() {
        assert_eq!(route(&timer(true, 0), false, false), Delivery::Orphan);
        assert_eq!(route(&timer(false, 0), false, false), Delivery::Orphan);
    }

    #[test]
    fn a_reminder_never_starts_a_turn() {
        assert_eq!(route(&timer(false, 0), true, false), Delivery::Notify);
        assert_eq!(route(&timer(false, 0), true, true), Delivery::Notify);
    }

    #[test]
    fn a_busy_chat_is_retried_then_degrades_to_a_reminder() {
        assert_eq!(route(&timer(true, 0), true, true), Delivery::Defer);
        assert_eq!(
            route(&timer(true, MAX_DEFERRALS - 1), true, true),
            Delivery::Defer
        );
        assert_eq!(
            route(&timer(true, MAX_DEFERRALS), true, true),
            Delivery::Notify
        );
    }

    #[test]
    fn an_idle_chat_is_woken() {
        assert_eq!(route(&timer(true, 0), true, false), Delivery::Wake);
    }

    #[test]
    fn the_wake_message_reads_as_a_timer_not_as_the_user() {
        let message = wake_message(&timer(true, 0));
        assert!(message.content.contains("not a message from the user"));
        assert!(message.content.contains("check the build"));
        assert_eq!(message.visible_content(), "⏰ timer #1: check the build");
        assert!(!message.ui_only, "the model must see the note");
    }

    #[test]
    fn a_throttled_reminder_says_why_it_did_not_resume() {
        assert!(!notice_text(&timer(false, 0), false).contains("not resumed"));
        assert!(notice_text(&timer(true, 99), true).contains("not resumed"));
    }
}
