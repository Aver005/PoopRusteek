//! Фасад шлюза для остального приложения: снимок, выбор цели с причиной, правки, учёт.
//!
//! Контракт: приложение, `/serve` и роли видят только `Gateway` и `Arc<dyn LLMProvider>`;
//! внутренности движка скрыты. Читать — `snapshot`, менять — `apply_edit`, слушать —
//! `subscribe`. `Decision` всегда объясняет выбор (идея A-01). Лимиты несут возраст и
//! источник: устаревшее и оценочное не участвует в жёстких решениях. Состояние cooldown
//! после перезапуска — только подсказка (`StateHint`). Реализует GW-ENGINE; здесь же
//! `DisabledGateway`: при `enabled = false` поведение прежнее.

use super::types::{Account, AccountId, GatewaySettings, Role, Roles, Route, RouteId, ToolMode};
use crate::config::Config;
use crate::provider::failure::{FailureClass, RateLimitHeaders};
use crate::provider::{LLMProvider, Usage};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

// ── Лимиты ──────────────────────────────────────────────────────────────

/// Откуда взято число. Только `Header` и `CliEvent` годятся для жёсткого решения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// Заголовки ответа API.
    Header,
    /// Событие самого CLI (Claude: `rate_limit_event`).
    CliEvent,
    /// Локальный счёт против заявленного бюджета: оценка.
    Ledger,
    /// Заявлено пользователем.
    Declared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Requests,
    Tokens,
    Spend,
    /// Сервер дал только проценты (подписки).
    Percent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitWindow {
    /// `5h`, `weekly`, `requests`, `tokens`, `spend`.
    pub name: String,
    pub kind: WindowKind,
    pub used_pct: Option<f32>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub resets_at: Option<SystemTime>,
}

/// Снимок лимитов аккаунта на момент `observed_at`.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitSnapshot {
    pub account: AccountId,
    pub plan: Option<String>,
    pub windows: Vec<LimitWindow>,
    pub observed_at: SystemTime,
    pub source: LimitSource,
}

impl LimitSnapshot {
    /// Из заголовков ответа. Процент считается, только если известны предел и остаток.
    pub fn from_headers(account: AccountId, headers: &RateLimitHeaders) -> Self {
        let windows = headers
            .windows
            .iter()
            .map(|w| {
                let used_pct = match (w.limit, w.remaining) {
                    (Some(limit), Some(remaining)) if limit > 0 => {
                        Some(100.0 - (remaining.min(limit) as f32 / limit as f32) * 100.0)
                    }
                    _ => None,
                };
                LimitWindow {
                    name: w.name.clone(),
                    kind: if w.name.contains("token") {
                        WindowKind::Tokens
                    } else {
                        WindowKind::Requests
                    },
                    used_pct,
                    limit: w.limit.map(|v| v as f64),
                    remaining: w.remaining.map(|v| v as f64),
                    resets_at: w.resets_at,
                }
            })
            .collect();
        Self {
            account,
            plan: None,
            windows,
            observed_at: headers.observed_at,
            source: LimitSource::Header,
        }
    }

    pub fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.observed_at)
            .unwrap_or(Duration::ZERO)
    }

    pub fn is_fresh(&self, now: SystemTime, ttl: Duration) -> bool {
        self.age(now) <= ttl
    }

    /// Можно ли опереться на снимок в жёстком решении (порог «заранее», отказ от аккаунта).
    pub fn is_hard_signal(&self, now: SystemTime, ttl: Duration) -> bool {
        matches!(self.source, LimitSource::Header | LimitSource::CliEvent)
            && self.is_fresh(now, ttl)
    }

    /// Самое занятое окно, в процентах.
    pub fn max_used_pct(&self) -> Option<f32> {
        self.windows
            .iter()
            .filter_map(|w| w.used_pct)
            .fold(None, |best, v| Some(best.map_or(v, |b: f32| b.max(v))))
    }
}

// ── Состояние аккаунта ──────────────────────────────────────────────────

/// Состояние аккаунта. Без `Draining` и `Open`: порог «заранее» — это порядок выбора.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AccountStatus {
    Ready,
    /// Лимит частоты: ждём `until`. Успешный вызов снимает раньше срока.
    Cooling {
        until: SystemTime,
        strikes: u32,
    },
    /// Квота или потолок расходов: до сброса окна или удачной пробы.
    Exhausted {
        resets_at: Option<SystemTime>,
    },
    /// Нужен человек: вход просрочен, аккаунт заблокирован.
    NeedsAttention {
        reason: String,
    },
    Disabled,
    /// Нет бинаря CLI, не поднялся адаптер.
    Unavailable {
        reason: String,
    },
}

impl AccountStatus {
    /// Не выбирается ли аккаунт в этот момент.
    pub fn blocks_selection(&self, now: SystemTime) -> bool {
        match self {
            Self::Ready => false,
            Self::Cooling { until, .. } => *until > now,
            Self::Exhausted { resets_at } => resets_at.is_none_or(|t| t > now),
            Self::NeedsAttention { .. } | Self::Disabled | Self::Unavailable { .. } => true,
        }
    }

    /// Когда аккаунт вернётся сам, если известно.
    pub fn returns_at(&self) -> Option<SystemTime> {
        match self {
            Self::Cooling { until, .. } => Some(*until),
            Self::Exhausted { resets_at } => *resets_at,
            _ => None,
        }
    }
}

/// То, что пережило перезапуск. Подсказка, а не состояние: первая проба решает.
#[derive(Debug, Clone, PartialEq)]
pub struct StateHint {
    pub account: AccountId,
    pub exhausted_until: Option<SystemTime>,
    pub saved_at: SystemTime,
}

impl StateHint {
    /// Срок, до которого аккаунт стоит ставить в конец очереди. Прошедший срок — `None`.
    pub fn soft_deadline(&self, now: SystemTime) -> Option<SystemTime> {
        self.exhausted_until.filter(|t| *t > now)
    }
}

// ── Выбор цели ──────────────────────────────────────────────────────────

/// Кто просит ответ. Нужен учёту и правилам (`/serve` без родных инструментов и т. п.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallSource {
    Chat,
    SubAgent,
    Role(Role),
    Serve,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub account: AccountId,
    pub model: String,
}

/// Почему шаг не взят.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SkipReason {
    Disabled,
    Cooling {
        until: SystemTime,
    },
    Exhausted {
        resets_at: Option<SystemTime>,
    },
    NeedsAttention,
    Unavailable,
    /// Занят: `max_concurrency` исчерпан, идём дальше, а не в очередь.
    Busy,
    /// Окно контекста меньше входа.
    ContextTooSmall {
        need: u32,
        have: u32,
    },
    /// Режим инструментов хода ему не подходит.
    ToolModeMismatch,
    /// CLI не работает как чистая модель.
    NotPureModel,
    /// Выше мягкого порога по свежему снимку.
    OverSoftLimit {
        used_pct: f32,
        age: Duration,
    },
    /// Выше жёсткого порога (`hard_pct`) по свежему снимку.
    OverHardLimit {
        used_pct: f32,
        age: Duration,
    },
    /// Подписки CLI наружу через `/serve` не раздаются.
    NotForServe,
    /// Ошибка конфига: аккаунт не используется, пока её не исправят.
    InvalidConfig {
        message: String,
    },
    /// В ходе есть файл, а аккаунт не умеет его приложить.
    NoAttachmentSupport,
    /// Уже упал в этом вызове.
    FailedThisCall {
        class: String,
    },
    /// Закреплён другой аккаунт.
    PinnedElsewhere,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub account: AccountId,
    pub reason: SkipReason,
}

/// Кто отвечает на вызов сейчас: для заголовков `/serve`. Ни секретов, ни адресов.
#[derive(Debug, Clone, PartialEq)]
pub struct ServedBy {
    pub target: Target,
    pub route: Option<RouteId>,
    /// Аккаунты, отказавшие раньше в этом вызове, и код класса отказа.
    pub failed_before: Vec<(AccountId, String)>,
}

/// Причина выбора.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Why {
    /// Первый подходящий по порядку шагов.
    FirstReady,
    /// Беседа держится за аккаунт ради кэша промпта.
    Sticky,
    /// Закреплён пользователем (`/pin`, `account/model`).
    Pinned,
    /// Предыдущая цель отказала в этом вызове.
    AfterFailure { from: AccountId, class: String },
    /// Все остальные отпали.
    OnlyCandidate,
    /// Выбирать некого.
    RouteExhausted,
    /// Маршрут или роль не используются из-за ошибки конфига.
    ConfigInvalid { message: String },
    /// Шлюз ещё собирается (секреты читаются вне цикла событий); запросы ждут готовности.
    Starting,
}

/// Запрос на выбор.
#[derive(Debug, Clone)]
pub struct DecisionQuery {
    pub role: Role,
    pub source: CallSource,
    pub tool_mode: ToolMode,
    /// Грубая оценка входа, токены; для фильтра по окну контекста.
    pub input_tokens: Option<u32>,
    pub pinned: Option<Target>,
    /// Где беседа была в прошлый раз.
    pub sticky: Option<AccountId>,
    /// Упавшие в этом вызове.
    pub failed: Vec<(AccountId, String)>,
}

/// Результат выбора с объяснением. Один тип для TUI, `/serve` и журнала.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub route: Option<RouteId>,
    pub target: Option<Target>,
    pub tool_mode: ToolMode,
    pub why: Why,
    pub skipped: Vec<Skipped>,
    /// Возраст снимка лимитов, на который опирались.
    pub snapshot_age: Option<Duration>,
    /// Если выбирать некого: когда вернётся ближайший.
    pub retry_at: Option<SystemTime>,
    pub at: SystemTime,
}

impl SkipReason {
    fn text(&self) -> String {
        match self {
            Self::Disabled => "switched off".into(),
            Self::Cooling { .. } => "cooling down after a rate limit".into(),
            Self::Exhausted { .. } => "quota exhausted".into(),
            Self::NeedsAttention => "needs attention (login or block)".into(),
            Self::Unavailable => "unavailable".into(),
            Self::Busy => "busy".into(),
            Self::ContextTooSmall { need, have } => {
                format!("context window {have} is below the {need} needed")
            }
            Self::ToolModeMismatch => "tool protocol does not match the turn".into(),
            Self::NotPureModel => "cannot run as a plain model".into(),
            Self::OverSoftLimit { used_pct, age } => {
                format!("at {used_pct:.0}% (reading {}s old)", age.as_secs())
            }
            Self::OverHardLimit { used_pct, age } => {
                format!(
                    "over its hard limit at {used_pct:.0}% (reading {}s old)",
                    age.as_secs()
                )
            }
            Self::NotForServe => "not served to outside clients".into(),
            Self::InvalidConfig { message } => format!("config error: {message}"),
            Self::NoAttachmentSupport => "cannot take the attached file".into(),
            Self::FailedThisCall { class } => format!("just failed ({class})"),
            Self::PinnedElsewhere => "another account is pinned".into(),
        }
    }
}

impl Decision {
    /// Одна строка для `/route why` и журнала решений.
    pub fn explain(&self) -> String {
        let head = match (&self.target, &self.why) {
            (Some(t), Why::FirstReady) => format!("{} · {}: first ready step", t.account, t.model),
            (Some(t), Why::Sticky) => format!("{} · {}: the chat stays on it", t.account, t.model),
            (Some(t), Why::Pinned) => format!("{} · {}: pinned", t.account, t.model),
            (Some(t), Why::AfterFailure { from, class }) => {
                format!("{} · {}: {from} failed ({class})", t.account, t.model)
            }
            (Some(t), _) => format!("{} · {}: the only candidate left", t.account, t.model),
            (None, Why::ConfigInvalid { message }) => format!("route is off: {message}"),
            (None, Why::Starting) => "the gateway is starting; requests wait for it".to_string(),
            (None, _) => "no account available".to_string(),
        };
        if self.skipped.is_empty() {
            return head;
        }
        let skipped: Vec<String> = self
            .skipped
            .iter()
            .map(|s| format!("{}: {}", s.account, s.reason.text()))
            .collect();
        format!("{head}; skipped {}", skipped.join(", "))
    }
}

// ── Учёт ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok,
    Failed(String),
    Cancelled,
}

/// Одна запись учёта. Повторы и fallback — отдельные записи одного `request_id`.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub at: SystemTime,
    pub source: CallSource,
    pub account: AccountId,
    pub model: String,
    pub route: Option<RouteId>,
    pub usage: Option<Usage>,
    /// Расход оценён, а не сообщён.
    pub estimated: bool,
    pub cost_usd: Option<f64>,
    pub latency_ms: u32,
    pub ttft_ms: Option<u32>,
    pub outcome: Outcome,
    pub fallback_from: Option<AccountId>,
    /// Номер попытки внутри запроса; различает «потрачено» и «доставлено».
    pub attempt: u8,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerQuery {
    pub account: Option<AccountId>,
    pub since: Option<SystemTime>,
}

/// Сумма по ключу `(аккаунт, модель, источник)`.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerRow {
    pub account: AccountId,
    pub model: String,
    pub source: CallSource,
    pub calls: u64,
    pub failed: u64,
    pub fallbacks: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    /// Часть строк — оценка.
    pub any_estimated: bool,
}

/// Интерфейс учёта. Хранение — агрегаты через `atomic_write` раз в ~30 с (ENGINE).
pub trait Ledger: Send + Sync {
    /// Быстро и без ввода-вывода: запись на диск делает фоновая задача.
    fn record(&self, record: UsageRecord);
    fn totals(&self, query: &LedgerQuery) -> Vec<LedgerRow>;
}

/// Учёт, который ничего не помнит.
pub struct NullLedger;

impl Ledger for NullLedger {
    fn record(&self, _record: UsageRecord) {}

    fn totals(&self, _query: &LedgerQuery) -> Vec<LedgerRow> {
        Vec::new()
    }
}

// ── Снимок, правки, события ─────────────────────────────────────────────

/// Аккаунт для показа. Секретов и ключей здесь нет.
#[derive(Debug, Clone)]
pub struct AccountView {
    pub id: AccountId,
    pub label: String,
    pub provider: super::types::AccountProvider,
    pub enabled: bool,
    pub status: AccountStatus,
    pub limits: Option<LimitSnapshot>,
    pub in_flight: u32,
    pub max_concurrency: u32,
    pub default_model: Option<String>,
    pub favorites: Vec<String>,
    pub capabilities: Option<super::backend::Capabilities>,
}

#[derive(Debug, Clone)]
pub struct GatewaySnapshot {
    pub settings: GatewaySettings,
    /// В порядке из конфига: порядок и есть приоритет.
    pub accounts: Vec<AccountView>,
    pub routes: Vec<Route>,
    pub roles: Roles,
    /// Последние решения, новые в конце.
    pub recent: Vec<Decision>,
    /// Замечания проверки конфига (ошибки и предупреждения); ошибки отключают аккаунт или маршрут.
    pub problems: Vec<super::types::Problem>,
    pub generated_at: SystemTime,
}

/// Единственный способ менять конфиг шлюза из интерфейса. Id не меняется никогда.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ConfigEdit {
    UpsertAccount(Box<Account>),
    RemoveAccount(AccountId),
    SetLabel { id: AccountId, label: String },
    SetEnabled { id: AccountId, enabled: bool },
    MoveAccount { id: AccountId, to_index: usize },
    UpsertRoute(Box<Route>),
    RemoveRoute(RouteId),
    SetRole { role: Role, route: Option<RouteId> },
    SetEnabledGateway(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    /// Результат не прошёл проверку конфига.
    Invalid(Vec<String>),
    NotFound(String),
    /// Нельзя записать (слой только для чтения, диск).
    Storage(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(items) => write!(f, "invalid config: {}", items.join("; ")),
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::Storage(why) => write!(f, "cannot save: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    Unknown(String),
    /// Шлюз выключен.
    Disabled,
    /// Шлюз ещё собирается: повторить через мгновение.
    Starting,
    /// Сборка не удалась; повтор не поможет, пока не исправят конфиг.
    Failed(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "unknown model or account '{id}'"),
            Self::Disabled => f.write_str("the gateway is disabled"),
            Self::Starting => f.write_str("the gateway is still starting"),
            Self::Failed(reason) => write!(f, "the gateway failed to start: {reason}"),
        }
    }
}

/// События для интерфейса (`AppEvent::Gateway`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GatewayEvent {
    AccountStatusChanged {
        account: AccountId,
        status: AccountStatus,
    },
    LimitsUpdated {
        account: AccountId,
    },
    /// Ушли с одного аккаунта на другой; показывается заметкой `ui_only`.
    FallbackTaken {
        from: AccountId,
        to: AccountId,
        class: FailureClass,
        source: CallSource,
    },
    ConfigChanged,
}

/// Готов ли шлюз: сборка идёт на `spawn_blocking`, пока она идёт или упала, снимок пуст.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildState {
    Ready,
    Starting,
    Failed(String),
}

/// Фасад. Реализует движок; приложение держит `GatewayHandle`.
pub trait Gateway: Send + Sync {
    fn is_enabled(&self) -> bool;

    /// Состояние сборки. Собранный и выключенный шлюзы всегда `Ready`.
    fn build_state(&self) -> BuildState {
        BuildState::Ready
    }
    fn snapshot(&self) -> GatewaySnapshot;

    /// Провайдер роли. Один `RoutedProvider` на вызывающего: у `fork()` общий движок и
    /// чистая привязка. `None` — роли нет, вызывающий берёт `build_provider` как раньше.
    fn provider_for_role(&self, role: Role, source: CallSource) -> Option<Arc<dyn LLMProvider>>;

    /// Провайдер по адресу: `account/model`, `auto/<route>`, голое имя модели.
    fn provider_for_model_id(
        &self,
        id: &str,
        source: CallSource,
    ) -> Result<Arc<dyn LLMProvider>, ResolveError>;

    /// Сухой прогон: что выберется сейчас и почему. Ничего не меняет.
    fn decide(&self, query: &DecisionQuery) -> Decision;

    fn apply_edit(&self, edit: ConfigEdit) -> Result<(), EditError>;
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GatewayEvent>;
    fn ledger(&self) -> Arc<dyn Ledger>;

    /// Живой адаптер аккаунта: каталог моделей (`model_cache`), проба. `None` у выключенного.
    fn backend_for(&self, _account: &AccountId) -> Option<Arc<dyn super::backend::Backend>> {
        None
    }
}

pub type GatewayHandle = Arc<dyn Gateway>;

/// Шлюз выключен: пустой снимок, ролей нет, правки не принимаются.
pub struct DisabledGateway {
    events: tokio::sync::broadcast::Sender<GatewayEvent>,
}

impl DisabledGateway {
    pub fn new() -> Self {
        Self {
            events: tokio::sync::broadcast::channel(1).0,
        }
    }
}

impl Default for DisabledGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway for DisabledGateway {
    fn is_enabled(&self) -> bool {
        false
    }

    fn snapshot(&self) -> GatewaySnapshot {
        GatewaySnapshot {
            settings: GatewaySettings::default(),
            accounts: Vec::new(),
            routes: Vec::new(),
            roles: Roles::default(),
            recent: Vec::new(),
            problems: Vec::new(),
            generated_at: SystemTime::now(),
        }
    }

    fn provider_for_role(&self, _role: Role, _source: CallSource) -> Option<Arc<dyn LLMProvider>> {
        None
    }

    fn provider_for_model_id(
        &self,
        _id: &str,
        _source: CallSource,
    ) -> Result<Arc<dyn LLMProvider>, ResolveError> {
        Err(ResolveError::Disabled)
    }

    fn decide(&self, query: &DecisionQuery) -> Decision {
        Decision {
            route: None,
            target: None,
            tool_mode: query.tool_mode,
            why: Why::RouteExhausted,
            skipped: Vec::new(),
            snapshot_age: None,
            retry_at: None,
            at: SystemTime::now(),
        }
    }

    fn apply_edit(&self, _edit: ConfigEdit) -> Result<(), EditError> {
        Err(EditError::Storage("the gateway is disabled".into()))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<GatewayEvent> {
        self.events.subscribe()
    }

    fn ledger(&self) -> Arc<dyn Ledger> {
        Arc::new(NullLedger)
    }
}

/// Главный провайдер для любой точки сборки (TUI, онбординг, харнесс, ACP, прокси).
/// Шлюз включён и ответил — берём его; иначе всё как раньше, `build_provider`.
pub fn main_provider(
    config: &Config,
    gateway: Option<&GatewayHandle>,
) -> Option<Arc<dyn LLMProvider>> {
    if let Some(gateway) = gateway
        && gateway.is_enabled()
        && let Some(provider) = gateway.provider_for_role(Role::Main, CallSource::Chat)
    {
        return Some(provider);
    }
    crate::provider::build_provider(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::failure::HeaderWindow;

    fn id(raw: &str) -> AccountId {
        AccountId::parse(raw).unwrap()
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_snapshot_knows_its_age_and_source() {
        let snap = LimitSnapshot {
            account: id("a"),
            plan: None,
            windows: Vec::new(),
            observed_at: at(1000),
            source: LimitSource::CliEvent,
        };
        let ttl = Duration::from_secs(60);
        assert!(snap.is_hard_signal(at(1030), ttl));
        assert!(
            !snap.is_hard_signal(at(1100), ttl),
            "stale data is not a hard signal"
        );
        let estimate = LimitSnapshot {
            source: LimitSource::Ledger,
            ..snap.clone()
        };
        assert!(
            !estimate.is_hard_signal(at(1010), ttl),
            "an estimate never is"
        );
        assert_eq!(
            snap.age(at(900)),
            Duration::ZERO,
            "clock going back is not negative age"
        );
    }

    #[test]
    fn header_windows_become_percentages() {
        let headers = RateLimitHeaders {
            retry_after: None,
            windows: vec![
                HeaderWindow {
                    name: "requests".into(),
                    limit: Some(100),
                    remaining: Some(25),
                    resets_at: None,
                },
                HeaderWindow {
                    name: "tokens".into(),
                    limit: Some(0),
                    remaining: Some(0),
                    resets_at: None,
                },
            ],
            observed_at: at(5),
        };
        let snap = LimitSnapshot::from_headers(id("a"), &headers);
        assert_eq!(snap.source, LimitSource::Header);
        assert_eq!(snap.windows[0].used_pct, Some(75.0));
        assert_eq!(snap.windows[1].kind, WindowKind::Tokens);
        assert_eq!(
            snap.windows[1].used_pct, None,
            "a zero limit gives no percentage"
        );
        assert_eq!(snap.max_used_pct(), Some(75.0));
    }

    #[test]
    fn cooling_ends_by_the_clock_but_unknown_exhaustion_waits_for_proof() {
        let cooling = AccountStatus::Cooling {
            until: at(100),
            strikes: 1,
        };
        assert!(cooling.blocks_selection(at(50)));
        assert!(!cooling.blocks_selection(at(100)), "the deadline frees it");
        let exhausted = AccountStatus::Exhausted { resets_at: None };
        assert!(
            exhausted.blocks_selection(at(10_000)),
            "no reset time, needs a probe or a success"
        );
        assert!(
            AccountStatus::NeedsAttention {
                reason: "login".into()
            }
            .blocks_selection(at(0))
        );
    }

    /// Анти-#5404: сохранённое состояние после перезапуска не блокирует навсегда.
    #[test]
    fn a_persisted_hint_expires_and_is_never_a_hard_state() {
        let hint = StateHint {
            account: id("a"),
            exhausted_until: Some(at(100)),
            saved_at: at(0),
        };
        assert_eq!(hint.soft_deadline(at(50)), Some(at(100)));
        assert_eq!(hint.soft_deadline(at(200)), None);
    }

    fn decision(target: Option<(&str, &str)>, why: Why, skipped: Vec<Skipped>) -> Decision {
        Decision {
            route: Some(RouteId::parse("coding").unwrap()),
            target: target.map(|(a, m)| Target {
                account: id(a),
                model: m.into(),
            }),
            tool_mode: ToolMode::Prompt,
            why,
            skipped,
            snapshot_age: None,
            retry_at: None,
            at: at(0),
        }
    }

    #[test]
    fn a_decision_explains_the_choice_and_what_was_skipped() {
        let d = decision(
            Some(("claude-personal", "opus")),
            Why::AfterFailure {
                from: id("claude-work"),
                class: "rate_limit".into(),
            },
            vec![Skipped {
                account: id("lmstudio"),
                reason: SkipReason::ContextTooSmall {
                    need: 50_000,
                    have: 8_192,
                },
            }],
        );
        let text = d.explain();
        assert!(text.contains("claude-personal · opus"), "{text}");
        assert!(text.contains("claude-work failed (rate_limit)"), "{text}");
        assert!(
            text.contains("lmstudio: context window 8192 is below the 50000 needed"),
            "{text}"
        );
        assert_eq!(
            decision(None, Why::RouteExhausted, vec![]).explain(),
            "no account available"
        );
    }

    #[test]
    fn a_disabled_gateway_changes_nothing_and_the_old_builder_is_used() {
        let gateway: GatewayHandle = Arc::new(DisabledGateway::new());
        assert!(!gateway.is_enabled());
        assert!(gateway.snapshot().accounts.is_empty());
        assert!(
            gateway
                .provider_for_role(Role::Main, CallSource::Chat)
                .is_none()
        );
        assert_eq!(
            gateway
                .provider_for_model_id("a/m", CallSource::Serve)
                .err(),
            Some(ResolveError::Disabled)
        );
        // Без токена и без записей старый путь тоже даёт `None`: поведение то же.
        let mut config = Config::default();
        config.provider.token.clear();
        assert!(main_provider(&config, Some(&gateway)).is_none());
        assert!(main_provider(&config, None).is_none());
    }

    #[test]
    fn the_ledger_interface_can_be_null() {
        let ledger: Arc<dyn Ledger> = Arc::new(NullLedger);
        ledger.record(UsageRecord {
            at: at(0),
            source: CallSource::Chat,
            account: id("a"),
            model: "m".into(),
            route: None,
            usage: None,
            estimated: true,
            cost_usd: None,
            latency_ms: 1,
            ttft_ms: None,
            outcome: Outcome::Ok,
            fallback_from: None,
            attempt: 1,
            request_id: None,
        });
        assert!(ledger.totals(&LedgerQuery::default()).is_empty());
    }
}
