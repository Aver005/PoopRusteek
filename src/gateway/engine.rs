//! Движок маршрутов: аккаунты в работе, выбор цели с причиной, cooldown, правки и события.
//! Замки `std::sync::Mutex` держатся только на короткие синхронные операции, не через `.await`.

use super::api_backend::ApiBackend;
use super::backend::{Backend, BackendError, BackendKind, Capabilities};
use super::clock::{Clock, SystemClock};
use super::deepseek_backend::DeepseekBackend;
use super::handle::{
    AccountStatus, AccountView, CallSource, ConfigEdit, Decision, DecisionQuery, DisabledGateway,
    EditError, Gateway, GatewayEvent, GatewayHandle, GatewaySnapshot, Ledger, LimitSnapshot,
    LimitSource, LimitWindow, ResolveError, SkipReason, Skipped, StateHint, Target, UsageRecord,
    Why, WindowKind,
};
use super::ledger::EngineLedger;
use super::routed::{RoutedProvider, failure, upstream_error};
use super::state::{AccountState, HintStore};
use super::types::{
    Account, AccountId, AccountLimits, AccountProvider, Credential, GatewayConfig, Problem, Role,
    Route, RouteId, SecretRef, Severity, Step, ToolMode, is_claude_host,
};
use crate::config::{Config, ProviderConfig, ToolProtocol};
use crate::error::AppError;
use crate::provider::LLMProvider;
use crate::provider::failure::FailureClass;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};
use tokio::sync::{Semaphore, broadcast};

/// Как часто фоновая задача сбрасывает учёт и подсказки на диск.
pub const FLUSH_EVERY: Duration = Duration::from_secs(30);
/// Дольше этого снимок лимитов не годится для жёсткого решения.
pub const LIMITS_TTL: Duration = Duration::from_secs(120);
/// Сколько последних решений помнит снимок.
const RECENT_LIMIT: usize = 32;

// ── Секреты: заглушка до `src/secrets/` ─────────────────────────────────

/// Расшифрованный секрет. Не печатается: ни в `Debug`, ни в ошибках.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Ошибка называет аккаунт и вид ссылки, но не значение.
#[derive(Debug, Clone, PartialEq)]
pub enum SecretError {
    Unsupported { account: String, scheme: String },
    Missing { account: String, name: String },
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { account, scheme } => write!(
                f,
                "account '{account}': secret scheme '{scheme}' needs the secret store"
            ),
            Self::Missing { account, name } => {
                write!(
                    f,
                    "account '{account}': environment variable {name} is not set"
                )
            }
        }
    }
}

/// Контракт SECRETS ↔ ENGINE. Настоящий резолвер живёт в `src/secrets/` и заменяет
/// `EnvOnlyResolver`; сигнатура та же, что в `.docs/gateway-contracts.md`.
pub trait SecretResolver: Send + Sync {
    /// Зовётся при сборке адаптера, не при разборе конфига.
    fn resolve(&self, reference: &SecretRef, account: &AccountId) -> Result<Secret, SecretError>;
}

/// Заглушка: понимает только `env:NAME` и `none`.
pub struct EnvOnlyResolver;

impl SecretResolver for EnvOnlyResolver {
    fn resolve(&self, reference: &SecretRef, account: &AccountId) -> Result<Secret, SecretError> {
        let raw = reference.as_str();
        if reference.is_none() {
            return Ok(Secret::new(String::new()));
        }
        match raw.strip_prefix("env:") {
            Some(name) => {
                std::env::var(name.trim())
                    .map(Secret::new)
                    .map_err(|_| SecretError::Missing {
                        account: account.to_string(),
                        name: name.trim().to_string(),
                    })
            }
            None => Err(SecretError::Unsupported {
                account: account.to_string(),
                scheme: raw.split(':').next().unwrap_or_default().to_string(),
            }),
        }
    }
}

// ── Сборка адаптеров ────────────────────────────────────────────────────

/// Фабрика адаптеров. Движок спрашивает фабрики по порядку; первая, взявшая аккаунт, побеждает.
/// GW-CLI добавляет свою для `cli:*`.
pub trait BackendFactory: Send + Sync {
    /// Вызывается один раз при создании движка, до сборки аккаунтов: можно прогреть кэши, которых
    /// не должно ждать позднейшее `apply_edit` (процессы, диск). Умолчание — ничего.
    fn prepare(&self, _accounts: &[Account]) {}

    /// `None` — аккаунт не этой фабрики.
    fn build(
        &self,
        account: &Account,
        secrets: &dyn SecretResolver,
    ) -> Option<Result<Arc<dyn Backend>, String>>;
}

/// Что нужно для встроенного DeepSeek: токен и лимитер лежат в `Config`, не в аккаунте.
#[derive(Clone)]
pub struct DeepseekSettings {
    provider: ProviderConfig,
    rate_limit_ms: u64,
    rate_limit_per_minute: u32,
    max_retries: i32,
}

impl DeepseekSettings {
    /// `None`, если токена нет: аккаунт `deepseek-web` тогда недоступен.
    pub fn from_config(config: &Config) -> Option<Self> {
        (!config.provider.token.is_empty()).then(|| Self {
            provider: config.provider.clone(),
            rate_limit_ms: config.agent.rate_limit_ms,
            rate_limit_per_minute: config.agent.rate_limit_per_minute,
            max_retries: config.agent.max_retries,
        })
    }
}

/// API-аккаунты и DeepSeek. CLI-аккаунты эта фабрика не берёт.
pub struct BuiltinFactory {
    deepseek: Option<DeepseekSettings>,
}

impl BuiltinFactory {
    pub fn new(deepseek: Option<DeepseekSettings>) -> Self {
        Self { deepseek }
    }
}

/// Ключ API-аккаунта: из конфига, из секрета или его нет вовсе.
fn resolve_api_key(
    account: &Account,
    secrets: &dyn SecretResolver,
) -> Result<Option<String>, String> {
    let from_secret = |reference: &SecretRef| {
        secrets
            .resolve(reference, &account.id)
            .map(|secret| Some(secret.expose().to_string()).filter(|key| !key.is_empty()))
            .map_err(|error| error.to_string())
    };
    match account.credential()? {
        // Последняя линия обороны: подписка Claude живёт только в официальном CLI.
        Credential::DirectToken(_)
            if account.provider.is_claude_family()
                || account.base_url.as_deref().is_some_and(is_claude_host) =>
        {
            Err("direct tokens are not allowed for Claude: use cli:claude".to_string())
        }
        Credential::DirectToken(_) if !account.direct_token_ack => {
            Err("direct_token needs direct_token_ack = true".to_string())
        }
        Credential::None => Ok(None),
        Credential::ApiKey(key) => Ok(Some(key).filter(|key| !key.is_empty())),
        Credential::SecretRef(reference) | Credential::DirectToken(reference) => {
            from_secret(&reference)
        }
        Credential::CliHome(_) => Err("home is only for cli:* accounts".to_string()),
    }
}

impl BackendFactory for BuiltinFactory {
    fn build(
        &self,
        account: &Account,
        secrets: &dyn SecretResolver,
    ) -> Option<Result<Arc<dyn Backend>, String>> {
        match account.provider {
            AccountProvider::Openai
            | AccountProvider::Anthropic
            | AccountProvider::Gemini
            | AccountProvider::Custom => Some(
                resolve_api_key(account, secrets)
                    .and_then(|key| ApiBackend::new(account, key))
                    .map(|backend| Arc::new(backend) as Arc<dyn Backend>),
            ),
            AccountProvider::DeepseekWeb => Some(self.deepseek_backend(account)),
            _ => None,
        }
    }
}

impl BuiltinFactory {
    fn deepseek_backend(&self, account: &Account) -> Result<Arc<dyn Backend>, String> {
        let settings = self
            .deepseek
            .as_ref()
            .ok_or_else(|| "no DeepSeek token in [provider]".to_string())?;
        let provider = crate::provider::deepseek::DeepseekProvider::new(
            &settings.provider,
            settings.rate_limit_ms,
            settings.rate_limit_per_minute,
            settings.max_retries,
        )
        .map_err(|error| error.to_string())?;
        Ok(Arc::new(DeepseekBackend::new(
            account.id.clone(),
            Arc::new(provider),
            settings.provider.model.clone(),
        )))
    }
}

/// Куда сохранять правки конфига шлюза. Слои конфига делает SECRETS; без неё правки
/// живут в памяти до перезапуска.
pub trait ConfigPersist: Send + Sync {
    fn persist(&self, config: &GatewayConfig) -> Result<(), String>;
}

pub struct EngineDeps {
    /// Папка `data_dir/gateway`; `None` — ничего на диск не пишем (тесты).
    pub data_dir: Option<PathBuf>,
    pub clock: Arc<dyn Clock>,
    pub resolver: Arc<dyn SecretResolver>,
    pub factories: Vec<Arc<dyn BackendFactory>>,
    pub persist: Option<Arc<dyn ConfigPersist>>,
}

impl EngineDeps {
    /// Рабочая сборка: данные в `data_dir/gateway`, DeepSeek и API-аккаунты.
    pub fn for_config(config: &Config) -> Self {
        Self {
            data_dir: Some(Config::data_dir().join("gateway")),
            clock: Arc::new(SystemClock),
            resolver: Arc::new(EnvOnlyResolver),
            factories: vec![
                Arc::new(BuiltinFactory::new(DeepseekSettings::from_config(config))),
                Arc::new(super::cli::CliFactory::new()),
            ],
            persist: None,
        }
    }
}

/// Откуда движок берёт `GatewayConfig`. Пока `[gateway]` не встроен в `Config` (дорожка
/// SECRETS), шлюз выключен: тело этой функции SECRETS заменит на чтение конфига.
pub fn gateway_config(_config: &Config) -> GatewayConfig {
    GatewayConfig::default()
}

/// Шлюз для приложения: выключенный — прежнее поведение без единого лишнего объекта.
pub fn build_gateway(gateway: GatewayConfig, deps: EngineDeps) -> GatewayHandle {
    if !gateway.gateway.enabled {
        return Arc::new(DisabledGateway::new());
    }
    Arc::new(Engine::new(gateway, deps))
}

/// Подписки CLI только для себя: в `/serve` в этом релизе их нет (ни Claude, ни OpenCode).
fn closed_to_serve(source: CallSource, provider: AccountProvider) -> bool {
    source == CallSource::Serve && provider.is_cli()
}

// ── Внутреннее устройство ───────────────────────────────────────────────

/// Аккаунт в работе: адаптер, семафор, состояние.
pub(super) struct Runtime {
    /// Только то, что меняет подключение (см. `connection_of`).
    pub connection: Account,
    /// Ошибка конфига этого аккаунта: он не используется, пока её не исправят.
    pub invalid: Option<String>,
    pub backend: Result<Arc<dyn Backend>, String>,
    pub caps: Option<Capabilities>,
    pub sem: Arc<Semaphore>,
    pub state: Mutex<AccountState>,
}

/// За что отвечает `RoutedProvider`: роль, маршрут по имени или один закреплённый аккаунт.
#[derive(Debug, Clone)]
pub(super) enum Binding {
    Role(Role),
    Route(RouteId),
    Fixed(Target),
}

/// Маршрут, развёрнутый в шаги на момент вызова.
pub(super) struct Plan {
    pub route: Option<Route>,
    pub steps: Vec<Step>,
    pub tool_mode: ToolMode,
}

pub(super) struct Selection {
    pub decision: Decision,
    /// Все годные заняты: ждём освобождения выбранного, а не ищем другой.
    pub queue: bool,
}

struct Candidate {
    target: Target,
    demoted: Option<SkipReason>,
    busy: bool,
    age: Option<Duration>,
}

/// Ошибки конфига, разложенные по тому, что они отключают.
#[derive(Default)]
pub(super) struct Issues {
    accounts: HashMap<AccountId, String>,
    routes: HashMap<RouteId, String>,
    roles: HashMap<Role, String>,
    pub problems: Vec<Problem>,
}

fn bracketed<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
    let rest = path.strip_prefix(prefix)?.strip_prefix('[')?;
    Some(&rest[..rest.find(']')?])
}

fn role_named(name: &str) -> Option<Role> {
    [Role::Main, Role::Title, Role::Summary, Role::Evaluator]
        .into_iter()
        .find(|role| format!("{role:?}") == name)
}

impl Issues {
    pub fn of(config: &GatewayConfig) -> Self {
        let mut issues = Self {
            problems: config.validate(),
            ..Self::default()
        };
        let add = |slot: &mut String, message: &str| {
            if !slot.is_empty() {
                slot.push_str("; ");
            }
            slot.push_str(message);
        };
        for problem in issues
            .problems
            .clone()
            .iter()
            .filter(|p| p.severity == Severity::Error)
        {
            if let Some(id) =
                bracketed(&problem.path, "accounts").and_then(|id| AccountId::parse(id).ok())
            {
                add(issues.accounts.entry(id).or_default(), &problem.message);
            } else if let Some(id) =
                bracketed(&problem.path, "routes").and_then(|id| RouteId::parse(id).ok())
            {
                add(issues.routes.entry(id).or_default(), &problem.message);
            } else if let Some(role) = problem.path.strip_prefix("roles.").and_then(role_named) {
                add(issues.roles.entry(role).or_default(), &problem.message);
            }
        }
        issues
    }

    pub fn errors(&self) -> Vec<String> {
        self.problems
            .iter()
            .filter(|p| p.severity == Severity::Error)
            .map(|p| format!("{}: {}", p.path, p.message))
            .collect()
    }
}

pub(super) struct Core {
    pub clock: Arc<dyn Clock>,
    resolver: Arc<dyn SecretResolver>,
    factories: Vec<Arc<dyn BackendFactory>>,
    persist: Option<Arc<dyn ConfigPersist>>,
    config: Mutex<Arc<GatewayConfig>>,
    runtimes: Mutex<HashMap<AccountId, Arc<Runtime>>>,
    recent: Mutex<VecDeque<Decision>>,
    events: broadcast::Sender<GatewayEvent>,
    pub ledger: Arc<EngineLedger>,
    windows: Mutex<HashMap<(AccountId, String), u32>>,
    hints: Option<HintStore>,
    saved_hints: Mutex<Vec<StateHint>>,
    /// Подсказки, снятые успехом или правкой, но ещё не стёртые на диске.
    cleared_hints: Mutex<Vec<(AccountId, SystemTime)>>,
    issues: Mutex<Arc<Issues>>,
    limits_ttl: Duration,
}

/// Поля, не влияющие на подключение: их правка не пересоздаёт адаптер и не сбрасывает cooldown.
fn connection_of(account: &Account) -> Account {
    let mut connection = account.clone();
    connection.label = None;
    connection.favorites.clear();
    connection.enabled = true;
    connection.limits = AccountLimits {
        max_concurrency: account.limits.max_concurrency,
        ..AccountLimits::default()
    };
    connection
}

fn tool_mode_of_account(account: &Account) -> ToolMode {
    match account.tool_protocol() {
        ToolProtocol::Native => ToolMode::Native,
        ToolProtocol::Prompt => ToolMode::Prompt,
    }
}

impl Core {
    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    pub fn config(&self) -> Arc<GatewayConfig> {
        self.config
            .lock()
            .map(|config| Arc::clone(&config))
            .unwrap_or_default()
    }

    pub fn runtime(&self, id: &AccountId) -> Option<Arc<Runtime>> {
        self.runtimes.lock().ok()?.get(id).cloned()
    }

    pub fn emit(&self, event: GatewayEvent) {
        // Нет подписчиков — не беда.
        let _ = self.events.send(event);
    }

    fn build_backend(&self, account: &Account) -> Result<Arc<dyn Backend>, String> {
        for factory in &self.factories {
            if let Some(built) = factory.build(account, self.resolver.as_ref()) {
                return built;
            }
        }
        Err(format!(
            "no adapter available for {:?} accounts in this build",
            account.provider
        ))
    }

    fn build_runtime(
        &self,
        account: &Account,
        hint: Option<StateHint>,
        invalid: Option<String>,
    ) -> Arc<Runtime> {
        let backend = match &invalid {
            Some(message) => Err(format!("config error: {message}")),
            None => self.build_backend(account),
        };
        if let Err(reason) = &backend {
            tracing::warn!("gateway account '{}' unavailable: {reason}", account.id);
        }
        let caps = backend.as_ref().ok().map(|backend| backend.capabilities());
        Arc::new(Runtime {
            connection: connection_of(account),
            invalid,
            backend,
            caps,
            // Нулевой семафор повесил бы каждый вызов; ноль в конфиге и так ошибка.
            sem: Arc::new(Semaphore::new(account.max_concurrency().max(1) as usize)),
            state: Mutex::new(AccountState::new(account.enabled, hint)),
        })
    }

    fn issues(&self) -> Arc<Issues> {
        self.issues
            .lock()
            .map(|issues| Arc::clone(&issues))
            .unwrap_or_default()
    }

    /// Почему привязка не используется из-за конфига, если это так.
    pub(super) fn binding_issue(&self, binding: &Binding) -> Option<String> {
        let issues = self.issues();
        let config = self.config();
        match binding {
            Binding::Role(role) => issues.roles.get(role).cloned().or_else(|| {
                let route = config.roles.route_for(*role)?;
                issues.routes.get(route).cloned()
            }),
            Binding::Route(route) => issues.routes.get(route).cloned(),
            Binding::Fixed(target) => issues.accounts.get(&target.account).cloned(),
        }
    }

    fn note_cleared(&self, account: &AccountId) {
        if let Ok(mut cleared) = self.cleared_hints.lock() {
            cleared.push((account.clone(), self.now()));
        }
    }

    /// Привести аккаунты в работе к конфигу: неизменившиеся остаются со своим состоянием.
    fn sync_runtimes(&self, config: &GatewayConfig, hints: &[StateHint], issues: &Issues) {
        let Ok(mut runtimes) = self.runtimes.lock() else {
            return;
        };
        let mut next: HashMap<AccountId, Arc<Runtime>> = HashMap::new();
        for account in &config.accounts {
            let invalid = issues.accounts.get(&account.id).cloned();
            let kept = runtimes
                .get(&account.id)
                .filter(|runtime| {
                    runtime.connection == connection_of(account) && runtime.invalid == invalid
                })
                .cloned();
            let runtime = match kept {
                Some(runtime) => {
                    if let Ok(mut state) = runtime.state.lock() {
                        let off = state.effective(self.now()) == AccountStatus::Disabled;
                        if off == account.enabled {
                            if state.hint.is_some() {
                                self.note_cleared(&account.id);
                            }
                            state.set_enabled(account.enabled);
                        }
                    }
                    runtime
                }
                None => {
                    let hint = hints.iter().find(|h| h.account == account.id).cloned();
                    self.build_runtime(account, hint, invalid)
                }
            };
            next.insert(account.id.clone(), runtime);
        }
        *runtimes = next;
    }

    /// План привязки; `None`, если маршрута нет или конфиг к нему не пускает.
    pub(super) fn plan(&self, binding: &Binding) -> Option<Plan> {
        if self.binding_issue(binding).is_some() {
            return None;
        }
        let config = self.config();
        match binding {
            Binding::Role(role) => {
                let route = config.route(config.roles.route_for(*role)?)?.clone();
                Self::plan_of_route(&config, route)
            }
            Binding::Route(id) => Self::plan_of_route(&config, config.route(id)?.clone()),
            Binding::Fixed(target) => {
                let account = config.account(&target.account)?;
                Some(Plan {
                    route: None,
                    steps: vec![Step {
                        account: target.account.clone(),
                        model: Some(target.model.clone()).filter(|m| !m.is_empty()),
                    }],
                    tool_mode: tool_mode_of_account(account),
                })
            }
        }
    }

    /// Смесь родных и промптовых инструментов без разрешения — ошибка, а не тихая смена режима.
    fn plan_of_route(config: &GatewayConfig, route: Route) -> Option<Plan> {
        let tool_mode = route.tool_mode(&config.accounts).ok()?;
        Some(Plan {
            steps: route.steps.clone(),
            route: Some(route),
            tool_mode,
        })
    }

    // ── Выбор ───────────────────────────────────────────────────────────

    fn evaluate(
        &self,
        config: &GatewayConfig,
        step: &Step,
        plan: &Plan,
        query: &DecisionQuery,
        attachments: &[PathBuf],
        now: SystemTime,
    ) -> (Option<SkipReason>, Option<Candidate>, Option<SystemTime>) {
        let skip = |reason| (Some(reason), None, None);
        let Some(account) = config.account(&step.account) else {
            return skip(SkipReason::Unavailable);
        };
        let model = query
            .pinned
            .as_ref()
            .filter(|pin| pin.account == account.id)
            .map(|pin| pin.model.clone())
            .or_else(|| step.model.clone())
            .or_else(|| account.model.clone())
            .unwrap_or_default();
        if !account.enabled {
            return skip(SkipReason::Disabled);
        }
        if let Some((_, class)) = query.failed.iter().find(|(id, _)| *id == account.id) {
            return skip(SkipReason::FailedThisCall {
                class: class.clone(),
            });
        }
        if closed_to_serve(query.source, account.provider) {
            return skip(SkipReason::NotForServe);
        }
        let Some(runtime) = self.runtime(&account.id) else {
            return skip(SkipReason::Unavailable);
        };
        if let Some(message) = &runtime.invalid {
            return skip(SkipReason::InvalidConfig {
                message: message.clone(),
            });
        }
        let (Some(caps), Ok(backend)) = (runtime.caps.as_ref(), runtime.backend.as_ref()) else {
            return skip(SkipReason::Unavailable);
        };
        let (status, returns_at, hint_deadline, limits) = match runtime.state.lock() {
            Ok(state) => (
                state.effective(now),
                state.returns_at(),
                state.hint_deadline(now),
                state.limits.clone(),
            ),
            Err(_) => return skip(SkipReason::Unavailable),
        };
        if status.blocks_selection(now) {
            let reason = match &status {
                AccountStatus::Cooling { until, .. } => SkipReason::Cooling { until: *until },
                AccountStatus::Exhausted { resets_at } => SkipReason::Exhausted {
                    resets_at: *resets_at,
                },
                AccountStatus::NeedsAttention { .. } => SkipReason::NeedsAttention,
                AccountStatus::Disabled => SkipReason::Disabled,
                _ => SkipReason::Unavailable,
            };
            return (Some(reason), None, returns_at);
        }
        if !caps.pure_model {
            return skip(SkipReason::NotPureModel);
        }
        // Адаптеру с HTTP-API нужна модель: пустое поле `model` сервер не примет.
        if model.is_empty() && matches!(backend.kind(), BackendKind::Api(_)) {
            return skip(SkipReason::InvalidConfig {
                message: "no model chosen: set model on the account or on the route step".into(),
            });
        }
        if attachments
            .iter()
            .any(|path| !backend.accepts_attachment(path))
        {
            return skip(SkipReason::NoAttachmentSupport);
        }
        if !caps.supports_tool_mode(query.tool_mode) {
            return skip(SkipReason::ToolModeMismatch);
        }
        if let (Some(need), Some(have)) = (query.input_tokens, self.window_of(&account.id, &model))
            && need > have
        {
            return skip(SkipReason::ContextTooSmall { need, have });
        }

        let mut demoted = None;
        let mut age = None;
        // Только свежий снимок от сервера: оценка и устаревшее числа не решают.
        if let Some(snapshot) = limits
            .as_ref()
            .filter(|snapshot| snapshot.is_hard_signal(now, self.limits_ttl))
            && let Some(used_pct) = snapshot.max_used_pct()
        {
            let snapshot_age = snapshot.age(now);
            age = Some(snapshot_age);
            if account
                .limits
                .hard_pct
                .is_some_and(|hard| used_pct >= f32::from(hard))
            {
                return skip(SkipReason::OverHardLimit {
                    used_pct,
                    age: snapshot_age,
                });
            }
            let soft = [
                account.limits.soft_pct,
                plan.route.as_ref().and_then(|r| r.switch_at_pct),
            ]
            .into_iter()
            .flatten()
            .min();
            if soft.is_some_and(|soft| used_pct >= f32::from(soft)) {
                demoted = Some(SkipReason::OverSoftLimit {
                    used_pct,
                    age: snapshot_age,
                });
            }
        }
        if demoted.is_none()
            && let Some(deadline) = hint_deadline
        {
            // Сохранённая подсказка: в конец очереди, но не запрет. Решает первый вызов.
            demoted = Some(SkipReason::Exhausted {
                resets_at: Some(deadline),
            });
        }
        let candidate = Candidate {
            target: Target {
                account: account.id.clone(),
                model,
            },
            demoted,
            busy: runtime.sem.available_permits() == 0,
            age,
        };
        (None, Some(candidate), None)
    }

    pub(super) fn window_of(&self, account: &AccountId, model: &str) -> Option<u32> {
        self.windows
            .lock()
            .ok()?
            .get(&(account.clone(), model.to_string()))
            .copied()
    }

    pub fn learn_window(&self, account: &AccountId, model: &str, window: u32) {
        if let Ok(mut windows) = self.windows.lock() {
            windows.insert((account.clone(), model.to_string()), window);
        }
    }

    pub fn select(
        &self,
        plan: &Plan,
        query: &DecisionQuery,
        route_id: Option<RouteId>,
    ) -> Selection {
        self.select_with(plan, query, route_id, &[])
    }

    /// Чистая функция выбора: ничего не меняет, кроме чтения состояния. `attachments` —
    /// файлы хода: брать можно только аккаунт, который их приложит.
    pub fn select_with(
        &self,
        plan: &Plan,
        query: &DecisionQuery,
        route_id: Option<RouteId>,
        attachments: &[PathBuf],
    ) -> Selection {
        let now = self.now();
        let config = self.config();
        let mut steps = plan.steps.clone();
        if let Some(pin) = &query.pinned
            && !steps.iter().any(|step| step.account == pin.account)
        {
            steps.push(Step {
                account: pin.account.clone(),
                model: Some(pin.model.clone()),
            });
        }

        let mut skipped: Vec<Skipped> = Vec::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut returns: Vec<SystemTime> = Vec::new();
        for step in &steps {
            if let Some(pin) = &query.pinned
                && pin.account != step.account
            {
                skipped.push(Skipped {
                    account: step.account.clone(),
                    reason: SkipReason::PinnedElsewhere,
                });
                continue;
            }
            let verdict = self.evaluate(&config, step, plan, query, attachments, now);
            match verdict {
                (Some(reason), _, at) => {
                    returns.extend(at);
                    skipped.push(Skipped {
                        account: step.account.clone(),
                        reason,
                    });
                }
                (None, Some(candidate), _) => candidates.push(candidate),
                (None, None, _) => {}
            }
        }

        let free = |c: &&Candidate| !c.busy;
        let sticky = query.sticky.as_ref().and_then(|id| {
            candidates
                .iter()
                .find(|c| c.target.account == *id && c.demoted.is_none() && !c.busy)
        });
        let chosen = sticky
            .or_else(|| candidates.iter().filter(free).find(|c| c.demoted.is_none()))
            .or_else(|| candidates.iter().find(|c| !c.busy))
            .or_else(|| candidates.iter().find(|c| c.demoted.is_none()))
            .or_else(|| candidates.first());
        let queue = chosen.is_some_and(|c| c.busy);

        for candidate in &candidates {
            if chosen.is_some_and(|c| c.target == candidate.target) {
                continue;
            }
            if let Some(reason) = &candidate.demoted {
                skipped.push(Skipped {
                    account: candidate.target.account.clone(),
                    reason: reason.clone(),
                });
            } else if candidate.busy && !queue {
                skipped.push(Skipped {
                    account: candidate.target.account.clone(),
                    reason: SkipReason::Busy,
                });
            }
        }

        let why = match (&query.pinned, chosen) {
            (_, None) => Why::RouteExhausted,
            (Some(_), Some(_)) => Why::Pinned,
            (None, Some(chosen)) => match query.failed.last() {
                Some((from, class)) => Why::AfterFailure {
                    from: from.clone(),
                    class: class.clone(),
                },
                None if query.sticky.as_ref() == Some(&chosen.target.account) => Why::Sticky,
                None if candidates.len() == 1 && plan.steps.len() > 1 => Why::OnlyCandidate,
                None => Why::FirstReady,
            },
        };
        let snapshot_age = chosen.and_then(|c| c.age).or_else(|| {
            skipped.iter().find_map(|s| match s.reason {
                SkipReason::OverHardLimit { age, .. } | SkipReason::OverSoftLimit { age, .. } => {
                    Some(age)
                }
                _ => None,
            })
        });
        let retry_at = if chosen.is_none() {
            skipped
                .iter()
                .filter_map(|s| match s.reason {
                    SkipReason::Cooling { until } => Some(until),
                    SkipReason::Exhausted { resets_at } => resets_at,
                    _ => None,
                })
                .chain(returns)
                .min()
        } else {
            None
        };
        Selection {
            queue,
            decision: Decision {
                route: route_id,
                target: chosen.map(|c| c.target.clone()),
                tool_mode: query.tool_mode,
                why,
                skipped,
                snapshot_age,
                retry_at,
                at: now,
            },
        }
    }

    pub fn ledger_record(&self, record: UsageRecord) {
        self.ledger.record(record);
    }

    /// Переходить ли к следующему шагу после такого отказа. Закреплённый аккаунт без запасных не переходит.
    pub fn fallback_allowed(&self, plan: &Plan, class: &FailureClass) -> bool {
        plan.route.as_ref().is_some_and(|route| {
            route.fallback_on(class)
                || matches!(
                    class,
                    FailureClass::ContextTooLong | FailureClass::ModelNotFound
                )
        })
    }

    /// Ошибка, когда выбирать некого. Была настоящая ошибка апстрима — отдаём её (класс сохранён,
    /// ожидание подправлено по ближайшему возврату); не было — «ждите до такого-то».
    pub fn exhausted_error(&self, decision: &Decision, last: Option<&BackendError>) -> AppError {
        let wait = decision
            .retry_at
            .map(|at| at.duration_since(self.now()).unwrap_or(Duration::ZERO));
        match (last, wait) {
            (Some(error), Some(wait)) => {
                let mut error = error.clone();
                if let FailureClass::RateLimit { retry_after } = &mut error.class {
                    *retry_after = Some(wait);
                }
                upstream_error(&error)
            }
            (Some(error), None) => upstream_error(error),
            (None, Some(wait)) => failure(
                FailureClass::RateLimit {
                    retry_after: Some(wait),
                },
                format!("no account available: {}", decision.explain()),
            ),
            (None, None) => failure(
                FailureClass::Overloaded,
                format!("no account available: {}", decision.explain()),
            ),
        }
    }

    pub fn remember_decision(&self, decision: &Decision) {
        if let Ok(mut recent) = self.recent.lock() {
            if recent.len() >= RECENT_LIMIT {
                recent.pop_front();
            }
            recent.push_back(decision.clone());
        }
    }

    // ── Итоги вызовов ───────────────────────────────────────────────────

    /// Свежий снимок лимитов от сервера. Событие — только когда он правда новый.
    pub fn note_limits(&self, runtime: &Runtime, account: &AccountId, snapshot: LimitSnapshot) {
        let changed = runtime.state.lock().is_ok_and(|mut state| {
            // Время наблюдения меняется на каждом ответе: событие только когда изменились сами числа.
            let same = state.limits.as_ref().is_some_and(|old| {
                old.windows == snapshot.windows
                    && old.source == snapshot.source
                    && old.plan == snapshot.plan
            });
            state.limits = Some(snapshot);
            !same
        });
        if changed {
            self.emit(GatewayEvent::LimitsUpdated {
                account: account.clone(),
            });
        }
    }

    pub fn report_success(&self, runtime: &Runtime, account: &AccountId) {
        let now = self.now();
        let (changed, had_hint) = runtime.state.lock().map_or((false, false), |mut state| {
            let had_hint = state.hint.is_some();
            (state.on_success(now), had_hint)
        });
        if had_hint {
            self.note_cleared(account);
        }
        if changed {
            self.emit(GatewayEvent::AccountStatusChanged {
                account: account.clone(),
                status: AccountStatus::Ready,
            });
        }
    }

    pub fn report_failure(
        &self,
        runtime: &Runtime,
        account: &AccountId,
        class: &FailureClass,
        message: &str,
    ) {
        let now = self.now();
        let outcome = runtime.state.lock().ok().map(|mut state| {
            let changed = state.on_failure(account, class, message, now);
            (changed, state.effective(now))
        });
        if let Some((true, status)) = outcome {
            self.emit(GatewayEvent::AccountStatusChanged {
                account: account.clone(),
                status,
            });
        }
    }

    // ── Диск ────────────────────────────────────────────────────────────

    fn live_hints(&self) -> Vec<StateHint> {
        let now = self.now();
        let Ok(runtimes) = self.runtimes.lock() else {
            return Vec::new();
        };
        let mut hints: Vec<StateHint> = runtimes
            .values()
            .filter_map(|runtime| {
                let state = runtime.state.lock().ok()?;
                state.hint_deadline(now)?;
                state.hint.clone()
            })
            .collect();
        hints.sort_by(|a, b| a.account.cmp(&b.account));
        hints
    }

    /// Сбросить учёт и подсказки на диск, слив их с записанным другими процессами.
    pub fn flush(&self) {
        if let Err(error) = self.ledger.flush() {
            tracing::warn!("gateway usage flush failed: {error}");
        }
        let Some(store) = &self.hints else { return };
        let hints = self.live_hints();
        let cleared = self
            .cleared_hints
            .lock()
            .map(|mut cleared| std::mem::take(&mut *cleared))
            .unwrap_or_default();
        let unchanged = self.saved_hints.lock().is_ok_and(|saved| *saved == hints);
        if unchanged && cleared.is_empty() {
            return;
        }
        match store.merge_save(&hints, &cleared, self.now()) {
            Ok(()) => {
                if let Ok(mut saved) = self.saved_hints.lock() {
                    *saved = hints;
                }
            }
            Err(error) => {
                tracing::warn!("gateway state flush failed: {error}");
                if let Ok(mut pending) = self.cleared_hints.lock() {
                    pending.extend(cleared);
                }
            }
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.flush();
    }
}

// ── Движок ──────────────────────────────────────────────────────────────

pub struct Engine {
    core: Arc<Core>,
}

impl Engine {
    pub fn new(config: GatewayConfig, deps: EngineDeps) -> Self {
        let hints_store = deps
            .data_dir
            .as_ref()
            .map(|dir| HintStore::new(dir.join("state.json")));
        let loaded_hints = hints_store
            .as_ref()
            .map(HintStore::load)
            .unwrap_or_default();
        let ledger = EngineLedger::new(deps.data_dir.as_ref().map(|dir| dir.join("usage.json")));
        let issues = Issues::of(&config);
        for message in issues.errors() {
            tracing::warn!("gateway config error: {message}");
        }
        for factory in &deps.factories {
            factory.prepare(&config.accounts);
        }
        let core = Arc::new(Core {
            clock: deps.clock,
            resolver: deps.resolver,
            factories: deps.factories,
            persist: deps.persist,
            config: Mutex::new(Arc::new(config.clone())),
            runtimes: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            events: broadcast::channel(64).0,
            ledger,
            windows: Mutex::new(HashMap::new()),
            hints: hints_store,
            saved_hints: Mutex::new(loaded_hints.clone()),
            cleared_hints: Mutex::new(Vec::new()),
            issues: Mutex::new(Arc::new(Issues::default())),
            limits_ttl: LIMITS_TTL,
        });
        core.sync_runtimes(&config, &loaded_hints, &issues);
        if let Ok(mut slot) = core.issues.lock() {
            *slot = Arc::new(issues);
        }
        Self::spawn_flusher(&core);
        Self { core }
    }

    /// Фоновая задача кончается сама, когда движок выброшен.
    fn spawn_flusher(core: &Arc<Core>) {
        if core.hints.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Core> = Arc::downgrade(core);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(FLUSH_EVERY).await;
                let Some(core) = weak.upgrade() else { break };
                let _ = tokio::task::spawn_blocking(move || core.flush()).await;
            }
        });
    }

    pub(super) fn core(&self) -> &Arc<Core> {
        &self.core
    }

    fn routed(&self, binding: Binding, source: CallSource, plan: &Plan) -> Arc<dyn LLMProvider> {
        Arc::new(RoutedProvider::new(
            Arc::clone(&self.core),
            binding,
            source,
            plan.tool_mode,
        ))
    }

    /// `account/model`, `auto/<route>`, имя аккаунта или голое имя модели.
    fn resolve(&self, id: &str) -> Option<Binding> {
        let config = self.core.config();
        if let Some((head, tail)) = id.split_once('/') {
            if head == "auto" {
                let route = RouteId::parse(tail).ok()?;
                return config.route(&route).map(|_| Binding::Route(route));
            }
            let account = AccountId::parse(head).ok()?;
            config.account(&account)?;
            return (!tail.is_empty()).then(|| {
                Binding::Fixed(Target {
                    account,
                    model: tail.to_string(),
                })
            });
        }
        if let Ok(account) = AccountId::parse(id)
            && let Some(found) = config.account(&account)
        {
            return Some(Binding::Fixed(Target {
                account,
                model: found.model.clone().unwrap_or_default(),
            }));
        }
        config
            .accounts
            .iter()
            .find(|a| a.model.as_deref() == Some(id) || a.favorites.iter().any(|f| f == id))
            .map(|a| {
                Binding::Fixed(Target {
                    account: a.id.clone(),
                    model: id.to_string(),
                })
            })
    }

    fn view_of(&self, account: &Account, now: SystemTime) -> AccountView {
        let runtime = self.core.runtime(&account.id);
        let (mut status, limits) = runtime
            .as_ref()
            .and_then(|r| {
                r.state
                    .lock()
                    .ok()
                    .map(|s| (s.effective(now), s.limits.clone()))
            })
            .unwrap_or((
                AccountStatus::Unavailable {
                    reason: "not started".into(),
                },
                None,
            ));
        if let Some(Err(reason)) = runtime.as_ref().map(|r| &r.backend)
            && status == AccountStatus::Ready
        {
            status = AccountStatus::Unavailable {
                reason: reason.clone(),
            };
        }
        let max = account.max_concurrency();
        let in_flight = runtime.as_ref().map_or(0, |r| {
            max.saturating_sub(r.sem.available_permits().min(max as usize) as u32)
        });
        AccountView {
            id: account.id.clone(),
            label: account.display_name().to_string(),
            provider: account.provider,
            enabled: account.enabled,
            status,
            limits: limits.or_else(|| self.declared_rate_estimate(account, now)),
            in_flight,
            max_concurrency: max,
            default_model: account.model.clone(),
            favorites: account.favorites.clone(),
            capabilities: runtime.and_then(|r| r.caps.clone()),
        }
    }

    /// Оценка по заявленному `requests_per_minute` из числа вызовов за минуту: источник
    /// `Ledger`, поэтому в жёстких решениях не участвует и в интерфейсе стоит с `~`.
    fn declared_rate_estimate(&self, account: &Account, now: SystemTime) -> Option<LimitSnapshot> {
        let per_minute = account.limits.requests_per_minute?;
        let since = now.checked_sub(Duration::from_secs(60))?;
        let calls: u64 = self
            .core
            .recent
            .lock()
            .ok()?
            .iter()
            .filter(|d| d.at >= since && d.target.as_ref().is_some_and(|t| t.account == account.id))
            .count() as u64;
        Some(LimitSnapshot {
            account: account.id.clone(),
            plan: None,
            windows: vec![LimitWindow {
                name: "requests/min".into(),
                kind: WindowKind::Requests,
                used_pct: Some((calls as f32 / per_minute.max(1) as f32 * 100.0).min(100.0)),
                limit: Some(f64::from(per_minute)),
                remaining: Some((f64::from(per_minute) - calls as f64).max(0.0)),
                resets_at: None,
            }],
            observed_at: now,
            source: LimitSource::Ledger,
        })
    }
}

impl Gateway for Engine {
    fn is_enabled(&self) -> bool {
        self.core.config().gateway.enabled
    }

    fn snapshot(&self) -> GatewaySnapshot {
        let now = self.core.now();
        let config = self.core.config();
        GatewaySnapshot {
            settings: config.gateway.clone(),
            accounts: config
                .accounts
                .iter()
                .map(|a| self.view_of(a, now))
                .collect(),
            routes: config.routes.clone(),
            roles: config.roles.clone(),
            recent: self
                .core
                .recent
                .lock()
                .map(|recent| recent.iter().cloned().collect())
                .unwrap_or_default(),
            problems: self.core.issues().problems.clone(),
            generated_at: now,
        }
    }

    fn provider_for_role(&self, role: Role, source: CallSource) -> Option<Arc<dyn LLMProvider>> {
        if !self.is_enabled() {
            return None;
        }
        let binding = Binding::Role(role);
        let plan = self.core.plan(&binding)?;
        Some(self.routed(binding, source, &plan))
    }

    fn provider_for_model_id(
        &self,
        id: &str,
        source: CallSource,
    ) -> Result<Arc<dyn LLMProvider>, ResolveError> {
        if !self.is_enabled() {
            return Err(ResolveError::Disabled);
        }
        let unknown = || ResolveError::Unknown(id.to_string());
        let binding = self.resolve(id).ok_or_else(unknown)?;
        // Подписки CLI наружу не раздаются: такой адрес для `/serve` не существует.
        if let Binding::Fixed(target) = &binding
            && self
                .core
                .config()
                .account(&target.account)
                .is_some_and(|a| closed_to_serve(source, a.provider))
        {
            return Err(unknown());
        }
        let plan = self.core.plan(&binding).ok_or_else(unknown)?;
        Ok(self.routed(binding, source, &plan))
    }

    fn decide(&self, query: &DecisionQuery) -> Decision {
        let binding = Binding::Role(query.role);
        match self.core.plan(&binding) {
            Some(plan) => {
                let route = self.core.config().roles.route_for(query.role).cloned();
                self.core.select(&plan, query, route).decision
            }
            None => Decision {
                route: None,
                target: None,
                tool_mode: query.tool_mode,
                why: match self.core.binding_issue(&binding) {
                    Some(message) => Why::ConfigInvalid { message },
                    None => Why::RouteExhausted,
                },
                skipped: Vec::new(),
                snapshot_age: None,
                retry_at: None,
                at: self.core.now(),
            },
        }
    }

    fn apply_edit(&self, edit: ConfigEdit) -> Result<(), EditError> {
        let mut next = (*self.core.config()).clone();
        apply_to_config(&mut next, edit)?;
        // Мешают только новые ошибки: чинить конфиг по одной правке можно и при старых.
        let issues = Issues::of(&next);
        let known = self.core.issues().errors();
        let introduced: Vec<String> = issues
            .errors()
            .into_iter()
            .filter(|error| !known.contains(error))
            .collect();
        if !introduced.is_empty() {
            return Err(EditError::Invalid(introduced));
        }
        if let Some(persist) = &self.core.persist {
            persist.persist(&next).map_err(EditError::Storage)?;
        }
        let hints = self.core.live_hints();
        self.core.sync_runtimes(&next, &hints, &issues);
        if let Ok(mut config) = self.core.config.lock() {
            *config = Arc::new(next);
        }
        if let Ok(mut slot) = self.core.issues.lock() {
            *slot = Arc::new(issues);
        }
        self.core.emit(GatewayEvent::ConfigChanged);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.core.events.subscribe()
    }

    fn ledger(&self) -> Arc<dyn Ledger> {
        Arc::clone(&self.core.ledger) as Arc<dyn Ledger>
    }

    fn backend_for(&self, account: &AccountId) -> Option<Arc<dyn Backend>> {
        self.core.runtime(account)?.backend.as_ref().ok().cloned()
    }
}

fn apply_to_config(config: &mut GatewayConfig, edit: ConfigEdit) -> Result<(), EditError> {
    let not_found = |what: &dyn std::fmt::Display| EditError::NotFound(what.to_string());
    match edit {
        ConfigEdit::UpsertAccount(account) => {
            match config.accounts.iter_mut().find(|a| a.id == account.id) {
                Some(existing) => *existing = *account,
                None => config.accounts.push(*account),
            }
        }
        ConfigEdit::RemoveAccount(id) => {
            let before = config.accounts.len();
            config.accounts.retain(|a| a.id != id);
            if config.accounts.len() == before {
                return Err(not_found(&id));
            }
        }
        ConfigEdit::SetLabel { id, label } => {
            let account = config
                .accounts
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or_else(|| not_found(&id))?;
            account.label = Some(label).filter(|l| !l.is_empty());
        }
        ConfigEdit::SetEnabled { id, enabled } => {
            config
                .accounts
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or_else(|| not_found(&id))?
                .enabled = enabled;
        }
        ConfigEdit::MoveAccount { id, to_index } => {
            let from = config
                .accounts
                .iter()
                .position(|a| a.id == id)
                .ok_or_else(|| not_found(&id))?;
            let account = config.accounts.remove(from);
            config
                .accounts
                .insert(to_index.min(config.accounts.len()), account);
        }
        ConfigEdit::UpsertRoute(route) => {
            match config.routes.iter_mut().find(|r| r.id == route.id) {
                Some(existing) => *existing = *route,
                None => config.routes.push(*route),
            }
        }
        ConfigEdit::RemoveRoute(id) => {
            let before = config.routes.len();
            config.routes.retain(|r| r.id != id);
            if config.routes.len() == before {
                return Err(not_found(&id));
            }
        }
        ConfigEdit::SetRole { role, route } => match route {
            Some(route) => {
                config.roles.0.insert(role, route);
            }
            None => {
                config.roles.0.remove(&role);
            }
        },
        ConfigEdit::SetEnabledGateway(enabled) => config.gateway.enabled = enabled,
    }
    Ok(())
}
