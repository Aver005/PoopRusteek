//! Отложенные задачи агента: «напомни / сделай это через 10 минут».
//!
//! Здесь живут только хранилище и разбор времени. Срабатывание — в
//! `app::timers` (тик главного цикла), разбор вызова — в
//! `agent::tools_step::manage_timer`: инструменту нужна беседа-владелец,
//! а её знает только ход.

use super::*;
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

/// Ниже этого порога таймер бессмысленен — короткое ожидание дешевле
/// сделать `sleep` в шелле, не отпуская ход.
pub const MIN_DELAY_SECS: i64 = 10;
/// Таймеры не переживают перезапуск, поэтому горизонт — сутки.
pub const MAX_HORIZON_SECS: i64 = 24 * 60 * 60;
/// Потолок живых таймеров на беседу: разгон ловится структурно, а не
/// уговорами в промпте.
pub const MAX_PER_OWNER: usize = 8;
/// Заметка едет в контекст при срабатывании — длинную режем.
pub const MAX_NOTE_BYTES: usize = 400;
/// Сколько ходов подряд беседу можно поднимать таймером без человека.
/// Дальше побудка вырождается в уведомление — иначе агент крутит сам себя.
pub const MAX_CONSECUTIVE_WAKES: u32 = 3;

/// Один взведённый таймер.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    pub id: u64,
    /// Беседа-владелец. `u64`, а не `ConversationId`: слой инструментов не
    /// знает про `app` (инвариант 6) — тип восстанавливает вызывающий.
    pub owner: u64,
    pub due_at: DateTime<Utc>,
    pub note: String,
    /// `true` — поднять ход агента, `false` — просто показать напоминание.
    pub wake: bool,
    /// Сколько раз срабатывание отложено из-за занятой беседы.
    pub deferrals: u32,
}

impl Timer {
    /// Строка для списка `/timers` и для ответа инструменту.
    pub fn describe(&self, now: DateTime<Utc>) -> String {
        let local = self.due_at.with_timezone(&chrono::Local);
        let mode = if self.wake { "wake" } else { "notify" };
        format!(
            "#{} — {} (in {}), {mode}: {}",
            self.id,
            local.format("%Y-%m-%d %H:%M"),
            format_left(self.due_at - now),
            self.note
        )
    }
}

/// «Через сколько» человеческими словами: `2h 5m`, `45s`, `now`.
fn format_left(left: Duration) -> String {
    let secs = left.num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m"),
    }
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    timers: Vec<Timer>,
    /// Беседа → сколько ходов подряд её подняли таймером.
    wakes: HashMap<u64, u32>,
}

/// Хранилище таймеров процесса. Один экземпляр живёт в `ToolRegistry`, его
/// же держит `App`. Мьютекс синхронный: под ним нет ни одного `.await`
/// (инвариант 2).
#[derive(Default)]
pub struct TimerStore {
    inner: Mutex<Inner>,
}

impl TimerStore {
    /// Взвести таймер. Отказ — это текст для модели, а не паника.
    pub fn set(
        &self,
        owner: u64,
        due_at: DateTime<Utc>,
        note: &str,
        wake: bool,
    ) -> Result<Timer, String> {
        let note = crate::util::truncate_at_char_boundary(note.trim(), MAX_NOTE_BYTES);
        if note.is_empty() {
            return Err("A timer needs a non-empty 'note'.".to_string());
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.timers.iter().filter(|t| t.owner == owner).count() >= MAX_PER_OWNER {
            return Err(format!(
                "This chat already has {MAX_PER_OWNER} pending timers. Cancel one first."
            ));
        }
        inner.next_id += 1;
        let timer = Timer {
            id: inner.next_id,
            owner,
            due_at,
            note: note.to_string(),
            wake,
            deferrals: 0,
        };
        inner.timers.push(timer.clone());
        Ok(timer)
    }

    /// Таймеры беседы (или все, когда `owner` не задан), по времени.
    pub fn list(&self, owner: Option<u64>) -> Vec<Timer> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<Timer> = inner
            .timers
            .iter()
            .filter(|t| owner.is_none_or(|o| t.owner == o))
            .cloned()
            .collect();
        out.sort_by_key(|t| t.due_at);
        out
    }

    /// Сколько таймеров взведено — для строки состояния.
    pub fn pending(&self) -> usize {
        self.inner.lock().unwrap().timers.len()
    }

    /// Снять таймер. `owner` ограничивает отмену своей беседой; `None` —
    /// отмена из `/timers`, где ограничивать нечем.
    pub fn cancel(&self, id: u64, owner: Option<u64>) -> Option<Timer> {
        let mut inner = self.inner.lock().unwrap();
        let index = inner
            .timers
            .iter()
            .position(|t| t.id == id && owner.is_none_or(|o| t.owner == o))?;
        Some(inner.timers.remove(index))
    }

    /// Снять всё, что принадлежит закрытой беседе.
    pub fn cancel_owner(&self, owner: u64) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.timers.len();
        inner.timers.retain(|t| t.owner != owner);
        inner.wakes.remove(&owner);
        before - inner.timers.len()
    }

    /// Забрать созревшие. Изымает их из хранилища: доставку решает
    /// вызывающий, а отложенный таймер он вернёт через `defer`.
    pub fn take_due(&self, now: DateTime<Utc>) -> Vec<Timer> {
        let mut inner = self.inner.lock().unwrap();
        if inner.timers.iter().all(|t| t.due_at > now) {
            return Vec::new();
        }
        let (due, pending): (Vec<Timer>, Vec<Timer>) =
            inner.timers.drain(..).partition(|t| t.due_at <= now);
        inner.timers = pending;
        due
    }

    /// Вернуть таймер в хранилище со сдвигом: целевая беседа была занята.
    pub fn defer(&self, mut timer: Timer, until: DateTime<Utc>) {
        timer.deferrals += 1;
        timer.due_at = until;
        self.inner.lock().unwrap().timers.push(timer);
    }

    /// Занять слот побудки. `false` — лимит подряд идущих побудок исчерпан.
    pub fn take_wake_slot(&self, owner: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let used = inner.wakes.entry(owner).or_insert(0);
        if *used >= MAX_CONSECUTIVE_WAKES {
            return false;
        }
        *used += 1;
        true
    }

    /// Человек написал в беседу — цепочка побудок оборвана.
    pub fn reset_wakes(&self, owner: u64) {
        self.inner.lock().unwrap().wakes.remove(&owner);
    }
}

/// Во сколько сработает таймер. Ровно один из `after` / `at`: два способа
/// сказать одно и то же в одном вызове — это ошибка модели, а не догадка.
pub fn resolve_due<Tz: TimeZone>(
    now: DateTime<Tz>,
    after: Option<&str>,
    at: Option<&str>,
) -> Result<DateTime<Utc>, String> {
    let due = match (after, at) {
        (Some(after), None) => now.clone() + parse_delay(after)?,
        (None, Some(at)) => parse_clock(now.clone(), at)?,
        (Some(_), Some(_)) => {
            return Err("Give either 'after' or 'at', not both.".to_string());
        }
        (None, None) => {
            return Err(
                "A timer needs 'after' (e.g. \"10m\") or 'at' (e.g. \"18:30\").".to_string(),
            );
        }
    };
    let left = due.clone() - now;
    if left < Duration::seconds(MIN_DELAY_SECS) {
        return Err(format!(
            "Too soon: a timer must be at least {MIN_DELAY_SECS}s away. \
            For a short wait use a sleep command in the shell instead."
        ));
    }
    if left > Duration::seconds(MAX_HORIZON_SECS) {
        return Err(
            "Too far: timers are limited to 24 hours and do not survive a restart.".to_string(),
        );
    }
    Ok(due.with_timezone(&Utc))
}

/// `"45s"`, `"20m"`, `"2h"`, голое число — секунды.
fn parse_delay(spec: &str) -> Result<Duration, String> {
    let spec = spec.trim().to_ascii_lowercase();
    let (digits, unit) = match spec.strip_suffix(['s', 'm', 'h']) {
        Some(digits) => (digits, spec.chars().last().unwrap_or('s')),
        None => (spec.as_str(), 's'),
    };
    let value: i64 = digits.trim().parse().map_err(|_| {
        format!("Cannot read delay '{spec}'. Use forms like \"45s\", \"20m\", \"2h\".")
    })?;
    Ok(match unit {
        'm' => Duration::minutes(value),
        'h' => Duration::hours(value),
        _ => Duration::seconds(value),
    })
}

/// `"18:30"` в часовом поясе `now`; прошедшее сегодня время — это завтра.
fn parse_clock<Tz: TimeZone>(now: DateTime<Tz>, at: &str) -> Result<DateTime<Tz>, String> {
    let at = at.trim();
    let time = NaiveTime::parse_from_str(at, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(at, "%H:%M:%S"))
        .map_err(|_| format!("Cannot read time '{at}'. Use 24h \"HH:MM\", e.g. \"18:30\"."))?;
    let today = now.date_naive().and_time(time);
    let tomorrow = today + Duration::days(1);
    for naive in [today, tomorrow] {
        // Несуществующее локальное время (переход на летнее) пропускаем.
        if let Some(candidate) = now.timezone().from_local_datetime(&naive).single()
            && candidate > now
        {
            return Ok(candidate);
        }
    }
    Err(format!("Cannot schedule '{at}' in this timezone."))
}

/// `timer` — схема для модели. Как и `task`, исполняется не через реестр:
/// вызов разбирает `agent::tools_step`, которому известна беседа.
pub struct TimerTool;

#[async_trait]
impl Tool for TimerTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: crate::tools::TIMER_TOOL_NAME.to_string(),
            description: "Schedule something for later: set a timer that fires in this chat. \
                Use it when the work or the reminder belongs to a later moment ('in 20 minutes', \
                'at 18:30'), not now. Not for waiting on a command — run that in the shell; \
                not for polling a background process — use shell_output. Timers are lost when \
                the app exits, so never promise more than 24 hours ahead."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["set", "list", "cancel"],
                        "description": "set (default), list this chat's timers, or cancel one by id."
                    },
                    "after": {
                        "type": "string",
                        "description": "Delay from now: \"45s\", \"20m\", \"2h\". Use this or 'at', not both."
                    },
                    "at": {
                        "type": "string",
                        "description": "Local wall-clock time, 24h \"HH:MM\". Past times mean tomorrow."
                    },
                    "note": {
                        "type": "string",
                        "description": "What to do or say when it fires. Write it self-contained — you will get this text and nothing else."
                    },
                    "wake": {
                        "type": "boolean",
                        "description": "false (default): show the note as a reminder. true: start a turn and act on it yourself — only when the user asked for an action, not a reminder."
                    },
                    "id": {
                        "type": "integer",
                        "description": "Timer id, for action=cancel."
                    }
                }
            }),
        }
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        ToolResult::error("Timers are unavailable in this run.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// Фиксированное «сейчас» с ненулевым смещением: разбор `at` обязан
    /// работать в часовом поясе вызывающего, а не в UTC машины.
    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-08-29T12:00:00+03:00").unwrap()
    }

    fn store_with(owner: u64, in_secs: i64, wake: bool) -> (TimerStore, Timer) {
        let store = TimerStore::default();
        let due = now().with_timezone(&Utc) + Duration::seconds(in_secs);
        let timer = store.set(owner, due, "check the build", wake).unwrap();
        (store, timer)
    }

    #[test]
    fn delay_forms_resolve_to_the_right_moment() {
        let base = now().with_timezone(&Utc);
        for (spec, secs) in [("45s", 45), ("20m", 20 * 60), ("2h", 2 * 3600), ("30", 30)] {
            let due = resolve_due(now(), Some(spec), None).unwrap();
            assert_eq!(due - base, Duration::seconds(secs), "spec {spec}");
        }
    }

    #[test]
    fn clock_time_already_past_today_means_tomorrow() {
        // 12:00 +03:00 сейчас; 09:00 сегодня прошло, значит завтра 09:00 = 06:00 UTC.
        let due = resolve_due(now(), None, Some("09:00")).unwrap();
        assert_eq!(due.to_rfc3339(), "2026-08-30T06:00:00+00:00");
    }

    #[test]
    fn clock_time_later_today_stays_today() {
        let due = resolve_due(now(), None, Some("18:30")).unwrap();
        assert_eq!(due.to_rfc3339(), "2026-08-29T15:30:00+00:00");
    }

    #[test]
    fn bounds_and_ambiguity_are_refused() {
        assert!(resolve_due(now(), Some("1s"), None).is_err());
        assert!(resolve_due(now(), Some("48h"), None).is_err());
        assert!(resolve_due(now(), Some("10m"), Some("18:30")).is_err());
        assert!(resolve_due(now(), None, None).is_err());
        assert!(resolve_due(now(), Some("soon"), None).is_err());
        assert!(resolve_due(now(), None, Some("25:00")).is_err());
    }

    #[test]
    fn take_due_returns_only_ripe_timers_and_removes_them() {
        let store = TimerStore::default();
        let base = now().with_timezone(&Utc);
        store
            .set(1, base + Duration::seconds(30), "soon", false)
            .unwrap();
        store
            .set(1, base + Duration::hours(2), "later", false)
            .unwrap();

        let due = store.take_due(base + Duration::minutes(1));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].note, "soon");
        assert_eq!(store.pending(), 1);
        assert!(store.take_due(base + Duration::minutes(1)).is_empty());
    }

    #[test]
    fn deferring_pushes_the_timer_forward_and_counts() {
        let (store, timer) = store_with(1, 30, true);
        let base = now().with_timezone(&Utc);
        let due = store.take_due(base + Duration::minutes(1)).pop().unwrap();
        store.defer(due, base + Duration::minutes(2));

        assert!(store.take_due(base + Duration::minutes(1)).is_empty());
        let again = store.take_due(base + Duration::minutes(3)).pop().unwrap();
        assert_eq!(again.id, timer.id);
        assert_eq!(again.deferrals, 1);
    }

    #[test]
    fn per_chat_limit_is_enforced() {
        let store = TimerStore::default();
        let base = now().with_timezone(&Utc);
        for i in 0..MAX_PER_OWNER {
            store
                .set(1, base + Duration::minutes(i as i64 + 1), "x", false)
                .unwrap();
        }
        assert!(store.set(1, base + Duration::hours(1), "x", false).is_err());
        // Лимит на беседу, а не на процесс.
        assert!(store.set(2, base + Duration::hours(1), "x", false).is_ok());
    }

    #[test]
    fn cancel_is_scoped_to_the_owner() {
        let (store, timer) = store_with(1, 60, false);
        assert!(store.cancel(timer.id, Some(2)).is_none());
        assert!(store.cancel(timer.id, Some(1)).is_some());
        assert_eq!(store.pending(), 0);
    }

    #[test]
    fn closing_a_chat_drops_its_timers_and_wake_budget() {
        let (store, _) = store_with(7, 60, true);
        assert!(store.take_wake_slot(7));
        assert_eq!(store.cancel_owner(7), 1);
        assert!(store.list(Some(7)).is_empty());
        // Бюджет побудок тоже сброшен — id беседы больше не переиспользуется.
        assert!(store.take_wake_slot(7));
    }

    #[test]
    fn consecutive_wakes_run_out_until_the_user_speaks() {
        let store = TimerStore::default();
        for _ in 0..MAX_CONSECUTIVE_WAKES {
            assert!(store.take_wake_slot(1));
        }
        assert!(!store.take_wake_slot(1));
        store.reset_wakes(1);
        assert!(store.take_wake_slot(1));
    }

    #[test]
    fn empty_note_is_refused_and_long_note_is_cut_on_a_char_boundary() {
        let store = TimerStore::default();
        let base = now().with_timezone(&Utc);
        assert!(
            store
                .set(1, base + Duration::minutes(1), "   ", false)
                .is_err()
        );

        let long = "мяу🐈".repeat(200);
        let timer = store
            .set(1, base + Duration::minutes(1), &long, false)
            .unwrap();
        assert!(timer.note.len() <= MAX_NOTE_BYTES);
        assert!(long.starts_with(&timer.note));
    }

    #[test]
    fn describe_reads_as_a_list_row() {
        let (_, timer) = store_with(1, 90, true);
        let text = timer.describe(now().with_timezone(&Utc));
        assert!(text.starts_with(&format!("#{}", timer.id)));
        assert!(text.contains("in 1m 30s"));
        assert!(text.contains("wake"));
        assert!(text.contains("check the build"));
    }
}
