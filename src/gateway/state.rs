//! Состояние аккаунта в движке: cooldown, возврат без человека, подсказки после перезапуска.
//!
//! Чистая логика без ввода-вывода (кроме `HintStore`) и без `.await`: время приходит
//! параметром, поэтому всё проверяется на ручных часах. Ничего здесь не блокирует
//! аккаунт навсегда: каждый запрет либо кончается по часам, либо снимается успехом.

use super::clock::{from_unix, to_unix};
use super::handle::{AccountStatus, LimitSnapshot, StateHint};
use super::types::{AccountId, RouteId};
use crate::provider::failure::FailureClass;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// `Retry-After` соблюдается всегда, но абсурдное значение не запирает аккаунт на недели.
pub const RETRY_AFTER_CAP: Duration = Duration::from_secs(24 * 3600);
/// Квота без известного сброса: через столько пробуем снова (первый вызов и есть проба).
pub const QUOTA_PROBE: Duration = Duration::from_secs(30 * 60);
pub const SPEND_CAP_PAUSE: Duration = Duration::from_secs(60 * 60);
/// Отключённый системой аккаунт возвращается на пробу сам (анти new-api #3537).
pub const AUTH_RECHECK: Duration = Duration::from_secs(10 * 60);
pub const BLOCKED_RECHECK: Duration = Duration::from_secs(60 * 60);

/// Где беседа остановилась в прошлый раз. Лежит в `Session.route_state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteId>,
    pub account: AccountId,
    pub model: String,
}

/// Хеш для разброса задержек: без `rand`, но у разных аккаунтов и попыток разный.
fn jitter_permille(seed: &str, strikes: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes().chain(strikes.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    800 + hash % 401
}

/// 5 с, 10 с, 20 с … до 5 минут, с разбросом ±20 %.
pub fn backoff(strikes: u32, seed: &str) -> Duration {
    let doublings = strikes.saturating_sub(1).min(16);
    let raw = BACKOFF_BASE
        .saturating_mul(1u32 << doublings)
        .min(BACKOFF_CAP);
    let jittered = raw.as_millis() as u64 * jitter_permille(seed, strikes) / 1000;
    Duration::from_millis(jittered).min(BACKOFF_CAP)
}

#[derive(Debug, Clone)]
pub struct AccountState {
    status: AccountStatus,
    strikes: u32,
    /// Когда `NeedsAttention` снова пробуется без человека.
    recheck_at: Option<SystemTime>,
    pub limits: Option<LimitSnapshot>,
    pub hint: Option<StateHint>,
}

impl AccountState {
    pub fn new(enabled: bool, hint: Option<StateHint>) -> Self {
        Self {
            status: if enabled {
                AccountStatus::Ready
            } else {
                AccountStatus::Disabled
            },
            strikes: 0,
            recheck_at: None,
            limits: None,
            hint,
        }
    }

    pub fn strikes(&self) -> u32 {
        self.strikes
    }

    /// Статус с учётом часов: истёкший запрет уже не запрет.
    pub fn effective(&self, now: SystemTime) -> AccountStatus {
        match &self.status {
            AccountStatus::Cooling { until, .. } if *until <= now => AccountStatus::Ready,
            AccountStatus::Exhausted {
                resets_at: Some(at),
            } if *at <= now => AccountStatus::Ready,
            AccountStatus::NeedsAttention { .. } if self.recheck_at.is_some_and(|t| t <= now) => {
                AccountStatus::Ready
            }
            other => other.clone(),
        }
    }

    /// Включить или выключить руками. Включение сбрасывает всё: это и есть «перепроверить».
    pub fn set_enabled(&mut self, enabled: bool) {
        self.strikes = 0;
        self.recheck_at = None;
        self.hint = None;
        self.status = if enabled {
            AccountStatus::Ready
        } else {
            AccountStatus::Disabled
        };
    }

    /// Успешный вызов снимает любой запрет раньше срока. `true` — видимый статус сменился.
    pub fn on_success(&mut self, now: SystemTime) -> bool {
        if self.status == AccountStatus::Disabled {
            return false;
        }
        let before = self.effective(now);
        self.status = AccountStatus::Ready;
        self.strikes = 0;
        self.recheck_at = None;
        self.hint = None;
        before != AccountStatus::Ready
    }

    /// Отказ апстрима. `true` — видимый статус сменился.
    pub fn on_failure(
        &mut self,
        id: &AccountId,
        class: &FailureClass,
        message: &str,
        now: SystemTime,
    ) -> bool {
        if self.status == AccountStatus::Disabled {
            return false;
        }
        let before = self.effective(now);
        let strikes = self.strikes + 1;
        match class {
            FailureClass::RateLimit { retry_after } => {
                let wait = match retry_after {
                    Some(wait) => (*wait).min(RETRY_AFTER_CAP),
                    None => backoff(strikes, id.as_str()),
                };
                self.cool(strikes, now + wait);
            }
            FailureClass::Overloaded
            | FailureClass::Timeout
            | FailureClass::Server
            | FailureClass::Network => {
                self.cool(strikes, now + backoff(strikes, id.as_str()));
            }
            FailureClass::QuotaExhausted { resets_at } => {
                let probe = now + QUOTA_PROBE;
                let until = resets_at
                    .filter(|at| *at > now)
                    .map_or(probe, |at| at.min(probe));
                self.exhaust(id, until, now);
            }
            FailureClass::SpendCap => self.exhaust(id, now + SPEND_CAP_PAUSE, now),
            FailureClass::Auth => self.need_attention(message, now + AUTH_RECHECK),
            FailureClass::AccountBlocked => self.need_attention(message, now + BLOCKED_RECHECK),
            // Плох сам запрос: аккаунт тут ни при чём.
            FailureClass::ContextTooLong
            | FailureClass::ModelNotFound
            | FailureClass::ContentFilter
            | FailureClass::BadRequest
            | FailureClass::Cancelled
            | FailureClass::Protocol => {}
        }
        before != self.effective(now)
    }

    fn cool(&mut self, strikes: u32, until: SystemTime) {
        self.strikes = strikes;
        self.recheck_at = None;
        self.status = AccountStatus::Cooling { until, strikes };
    }

    fn exhaust(&mut self, id: &AccountId, until: SystemTime, now: SystemTime) {
        self.recheck_at = None;
        self.status = AccountStatus::Exhausted {
            resets_at: Some(until),
        };
        self.hint = Some(StateHint {
            account: id.clone(),
            exhausted_until: Some(until),
            saved_at: now,
        });
    }

    fn need_attention(&mut self, message: &str, recheck_at: SystemTime) {
        self.recheck_at = Some(recheck_at);
        self.status = AccountStatus::NeedsAttention {
            reason: message.to_string(),
        };
    }

    /// Когда запрет снимется сам, если это известно (нужно для `Decision::retry_at`).
    pub fn returns_at(&self) -> Option<SystemTime> {
        match &self.status {
            AccountStatus::Cooling { until, .. } => Some(*until),
            AccountStatus::Exhausted { resets_at } => *resets_at,
            AccountStatus::NeedsAttention { .. } => self.recheck_at,
            _ => None,
        }
    }

    /// До какого срока аккаунт стоит ставить в конец очереди по сохранённой подсказке.
    pub fn hint_deadline(&self, now: SystemTime) -> Option<SystemTime> {
        self.hint.as_ref().and_then(|hint| hint.soft_deadline(now))
    }
}

// ── Подсказки на диске ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct HintRow {
    account: String,
    exhausted_until: Option<u64>,
    saved_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HintFile {
    version: u32,
    hints: Vec<HintRow>,
}

/// `data_dir/gateway/state.json`. После перезапуска читается как подсказка, не как состояние.
pub struct HintStore {
    path: PathBuf,
}

impl HintStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Нет файла или он битый — подсказок нет; запрета это не создаёт.
    pub fn load(&self) -> Vec<StateHint> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let file: HintFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!("gateway state hints ignored: {error}");
                return Vec::new();
            }
        };
        file.hints
            .into_iter()
            .filter_map(|row| {
                Some(StateHint {
                    account: AccountId::parse(&row.account).ok()?,
                    exhausted_until: row.exhausted_until.map(from_unix),
                    saved_at: from_unix(row.saved_at),
                })
            })
            .collect()
    }

    pub fn save(&self, hints: &[StateHint]) -> std::io::Result<()> {
        let file = HintFile {
            version: 1,
            hints: hints
                .iter()
                .map(|hint| HintRow {
                    account: hint.account.to_string(),
                    exhausted_until: hint.exhausted_until.map(to_unix),
                    saved_at: to_unix(hint.saved_at),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        crate::util::atomic_write(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::clock::scratch_dir;
    use super::*;

    fn id(raw: &str) -> AccountId {
        AccountId::parse(raw).unwrap()
    }

    fn at(secs: u64) -> SystemTime {
        from_unix(secs)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn fail(state: &mut AccountState, class: FailureClass, now: u64) -> bool {
        state.on_failure(&id("a"), &class, "boom", at(now))
    }

    #[test]
    fn retry_after_is_honoured_to_the_second_and_ends_by_the_clock() {
        let mut state = AccountState::new(true, None);
        let changed = fail(
            &mut state,
            FailureClass::RateLimit {
                retry_after: Some(secs(40)),
            },
            1_000,
        );
        assert!(changed);
        assert_eq!(
            state.effective(at(1_039)),
            AccountStatus::Cooling {
                until: at(1_040),
                strikes: 1
            }
        );
        assert_eq!(state.effective(at(1_040)), AccountStatus::Ready);
    }

    #[test]
    fn a_rate_limit_without_a_hint_backs_off_harder_each_time_up_to_a_cap() {
        let mut waits = Vec::new();
        let mut state = AccountState::new(true, None);
        for _ in 0..12 {
            fail(&mut state, FailureClass::RateLimit { retry_after: None }, 0);
            let AccountStatus::Cooling { until, .. } = state.effective(at(0)) else {
                panic!("should be cooling");
            };
            waits.push(until.duration_since(at(0)).unwrap());
        }
        assert!(waits[0] >= secs(4) && waits[0] <= secs(6), "{:?}", waits[0]);
        assert!(waits[3] > waits[0] * 4, "doubles: {waits:?}");
        assert!(waits.iter().all(|w| *w <= BACKOFF_CAP), "{waits:?}");
        assert!(waits[11] >= secs(240), "reaches the cap: {:?}", waits[11]);
    }

    #[test]
    fn a_success_lifts_the_cooldown_early_and_forgets_the_strikes() {
        let mut state = AccountState::new(true, None);
        fail(
            &mut state,
            FailureClass::RateLimit {
                retry_after: Some(secs(600)),
            },
            0,
        );
        assert!(state.effective(at(1)).blocks_selection(at(1)));
        assert!(state.on_success(at(1)), "the visible status changed");
        assert_eq!(state.effective(at(1)), AccountStatus::Ready);
        assert_eq!(state.strikes(), 0);
        assert!(!state.on_success(at(2)), "nothing changed the second time");
    }

    #[test]
    fn quota_is_probed_again_in_half_an_hour_even_when_the_reset_is_unknown_or_far() {
        let mut unknown = AccountState::new(true, None);
        fail(
            &mut unknown,
            FailureClass::QuotaExhausted { resets_at: None },
            0,
        );
        assert!(unknown.effective(at(1_799)).blocks_selection(at(1_799)));
        assert_eq!(unknown.effective(at(1_800)), AccountStatus::Ready);

        let mut far = AccountState::new(true, None);
        fail(
            &mut far,
            FailureClass::QuotaExhausted {
                resets_at: Some(at(7 * 24 * 3600)),
            },
            0,
        );
        assert_eq!(
            far.effective(at(1_800)),
            AccountStatus::Ready,
            "never trust a distant reset"
        );

        let mut near = AccountState::new(true, None);
        fail(
            &mut near,
            FailureClass::QuotaExhausted {
                resets_at: Some(at(120)),
            },
            0,
        );
        assert_eq!(
            near.effective(at(120)),
            AccountStatus::Ready,
            "a near reset is used as is"
        );
        assert!(near.effective(at(119)).blocks_selection(at(119)));
    }

    #[test]
    fn a_spend_cap_pauses_for_an_hour_and_leaves_a_hint() {
        let mut state = AccountState::new(true, None);
        fail(&mut state, FailureClass::SpendCap, 0);
        assert!(state.effective(at(3_599)).blocks_selection(at(3_599)));
        assert_eq!(state.effective(at(3_600)), AccountStatus::Ready);
        assert_eq!(
            state.hint.as_ref().unwrap().exhausted_until,
            Some(at(3_600))
        );
    }

    /// Анти new-api #3537: аккаунт, отключённый системой, возвращается на пробу сам.
    #[test]
    fn an_account_the_system_put_aside_comes_back_by_itself() {
        let mut state = AccountState::new(true, None);
        fail(&mut state, FailureClass::Auth, 0);
        assert!(matches!(
            state.effective(at(599)),
            AccountStatus::NeedsAttention { .. }
        ));
        assert_eq!(state.effective(at(600)), AccountStatus::Ready);

        let mut blocked = AccountState::new(true, None);
        fail(&mut blocked, FailureClass::AccountBlocked, 0);
        assert!(blocked.effective(at(3_599)).blocks_selection(at(3_599)));
        assert_eq!(blocked.effective(at(3_600)), AccountStatus::Ready);
    }

    /// Отрицательная сторона: то, что выключил человек, само не включается.
    #[test]
    fn what_the_user_switched_off_stays_off_until_the_user_says_otherwise() {
        let mut state = AccountState::new(false, None);
        assert_eq!(state.effective(at(0)), AccountStatus::Disabled);
        assert!(!state.on_success(at(1)));
        assert!(!fail(&mut state, FailureClass::Auth, 2));
        assert_eq!(state.effective(at(10_000_000)), AccountStatus::Disabled);
        state.set_enabled(true);
        assert_eq!(state.effective(at(3)), AccountStatus::Ready);
    }

    #[test]
    fn a_bad_request_says_nothing_about_the_account() {
        let mut state = AccountState::new(true, None);
        for class in [
            FailureClass::BadRequest,
            FailureClass::ContentFilter,
            FailureClass::ContextTooLong,
            FailureClass::ModelNotFound,
            FailureClass::Cancelled,
            FailureClass::Protocol,
        ] {
            assert!(!fail(&mut state, class, 0));
        }
        assert_eq!(state.effective(at(0)), AccountStatus::Ready);
        assert_eq!(state.strikes(), 0);
    }

    #[test]
    fn hints_survive_a_restart_as_hints_and_expire() {
        let dir = scratch_dir("hints");
        let store = HintStore::new(dir.join("gateway").join("state.json"));
        assert!(store.load().is_empty(), "no file, no hints");
        let hint = StateHint {
            account: id("a"),
            exhausted_until: Some(at(500)),
            saved_at: at(100),
        };
        store.save(std::slice::from_ref(&hint)).unwrap();
        let loaded = store.load();
        assert_eq!(loaded, vec![hint]);
        let state = AccountState::new(true, loaded.into_iter().next());
        assert_eq!(
            state.effective(at(200)),
            AccountStatus::Ready,
            "a hint never blocks"
        );
        assert_eq!(state.hint_deadline(at(200)), Some(at(500)));
        assert_eq!(state.hint_deadline(at(500)), None, "an old hint is ignored");
    }

    #[test]
    fn a_corrupt_hint_file_is_ignored_not_fatal() {
        let dir = scratch_dir("hints");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(HintStore::new(path).load().is_empty());
    }

    #[test]
    fn route_state_round_trips_and_an_old_session_without_it_still_loads() {
        let state = RouteState {
            route: Some(RouteId::parse("coding").unwrap()),
            account: id("claude-work"),
            model: "opus".into(),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<RouteState>(&json).unwrap(), state);
        let no_route: RouteState = serde_json::from_str(r#"{"account":"a","model":"m"}"#).unwrap();
        assert!(no_route.route.is_none());
    }
}
