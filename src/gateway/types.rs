//! Данные шлюза: аккаунты, маршруты, роли, способ входа, проверка конфига.
//!
//! Контракт файла: только типы с serde, чистые функции и проверки. Ни ввода-вывода,
//! ни `.await`. Схема TOML повторяет `02-gateway-architecture.md` §3.3: `[gateway]`,
//! `[[accounts]]`, `[[routes]]`, `[roles]`. Встраивание в `Config` делает SECRETS.
//! Владелец: GW-0 (этот файл), дальше только аддитивные правки (`#[non_exhaustive]`).
//!
//! Чего в релизе нет: `spread`, маршрутов внутри маршрутов, `Draining`/half-open.

use crate::config::{ProviderEntry, ProviderProtocol, ToolProtocol};
use crate::provider::failure::FailureClass;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

// ── Идентификаторы ──────────────────────────────────────────────────────

/// Стабильный ключ аккаунта: на него ссылаются маршруты, секреты и сессии.
/// Не меняется при переименовании (`label` — отдельно).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

/// Идентификатор маршрута (`auto/<id>` в `/serve`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RouteId(String);

/// Предел длины идентификатора.
const ID_MAX_LEN: usize = 64;

/// Что не так с идентификатором, если что-то не так.
fn id_problem(raw: &str) -> Option<&'static str> {
    if raw.is_empty() {
        Some("id is empty")
    } else if raw.chars().count() > ID_MAX_LEN {
        Some("id is longer than 64 characters")
    } else if raw
        .chars()
        .any(|c| c == '/' || c.is_control() || c.is_whitespace())
    {
        Some("id must not contain '/', spaces or control characters")
    } else {
        None
    }
}

macro_rules! id_impl {
    ($name:ident) => {
        impl $name {
            /// Принимает всё, что не ломает адреса `account/model`. Старые имена
            /// записей `[[providers]]` проходят как есть: на них держится `entry/model`.
            pub fn parse(raw: &str) -> Result<Self, String> {
                match id_problem(raw) {
                    Some(problem) => Err(format!("'{raw}': {problem}")),
                    None => Ok(Self(raw.to_string())),
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Slug для новых идентификаторов: `a-z`, цифры, `-`, `_`, с буквы.
            pub fn is_slug(&self) -> bool {
                let mut chars = self.0.chars();
                chars.next().is_some_and(|c| c.is_ascii_lowercase())
                    && chars.all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
                    })
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_impl!(AccountId);
id_impl!(RouteId);

// ── Провайдер и способ входа ────────────────────────────────────────────

/// Какой адаптер обслуживает аккаунт. Строки — как в конфиге.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AccountProvider {
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "gemini")]
    Gemini,
    /// Любой OpenAI-совместимый сервер со своим `base_url`.
    #[serde(rename = "custom")]
    Custom,
    /// Встроенный веб-клиент DeepSeek. В релизе он один: токен лежит в `[provider]`.
    #[serde(rename = "deepseek-web")]
    DeepseekWeb,
    #[serde(rename = "cli:codex")]
    CliCodex,
    #[serde(rename = "cli:claude")]
    CliClaude,
    #[serde(rename = "cli:opencode")]
    CliOpencode,
}

/// Официальный CLI подписки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CliTool {
    Claude,
    Opencode,
    Codex,
}

impl CliTool {
    /// Переменная окружения, которая выбирает аккаунт CLI (`06-cli-spike.md`).
    /// У OpenCode это каталог данных; каталог конфигурации адаптер подменяет сам.
    pub fn home_env_var(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Opencode => "XDG_DATA_HOME",
            Self::Codex => "CODEX_HOME",
        }
    }

    /// Проверено ли, что CLI можно превратить в «чистую модель» (инструменты
    /// выключены). Claude и OpenCode — да; Codex — нет (`06-cli-spike.md` §3).
    pub fn pure_model_verified(self) -> bool {
        !matches!(self, Self::Codex)
    }
}

/// Способ транспорта CLI. Нужен адаптеру, не конфигу.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CliTransport {
    /// Процесс на вызов: `claude -p`, `opencode run`.
    OneShot,
    /// Долгоживущий JSON-RPC (`codex app-server`). Не используется в релизе.
    JsonRpc,
}

impl AccountProvider {
    pub fn cli_tool(self) -> Option<CliTool> {
        match self {
            Self::CliClaude => Some(CliTool::Claude),
            Self::CliOpencode => Some(CliTool::Opencode),
            Self::CliCodex => Some(CliTool::Codex),
            _ => None,
        }
    }

    /// Протокол провода для HTTP-аккаунтов; у CLI и DeepSeek его нет.
    pub fn protocol(self) -> Option<ProviderProtocol> {
        match self {
            Self::Openai | Self::Custom => Some(ProviderProtocol::Openai),
            Self::Anthropic => Some(ProviderProtocol::Anthropic),
            Self::Gemini => Some(ProviderProtocol::Gemini),
            _ => None,
        }
    }

    /// Подписка Claude: Anthropic API и Claude Code. Прямые токены тут запрещены.
    pub fn is_claude_family(self) -> bool {
        matches!(self, Self::Anthropic | Self::CliClaude)
    }
}

/// Ссылка на секрет: `keyring:<id>`, `env:NAME`, `cmd:[…]`, `file:/путь`, `none`.
/// Здесь проверяется только форма; разбор и чтение — в `src/secrets/` (SECRETS).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretRef(String);

impl SecretRef {
    const SCHEMES: [&'static str; 4] = ["keyring:", "env:", "cmd:", "file:"];

    pub fn parse(raw: &str) -> Result<Self, String> {
        let body = raw.trim();
        let known = body == "none" || Self::SCHEMES.iter().any(|s| body.starts_with(s));
        let has_value = Self::SCHEMES
            .iter()
            .find_map(|s| body.strip_prefix(s))
            .is_none_or(|rest| !rest.trim().is_empty());
        if known && has_value {
            Ok(Self(body.to_string()))
        } else {
            // Значение в сообщение не попадает: это мог быть сам ключ.
            Err("secret must be none, keyring:<id>, env:NAME, cmd:[…] or file:<path>".to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_none(&self) -> bool {
        self.0 == "none"
    }
}

impl TryFrom<String> for SecretRef {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<SecretRef> for String {
    fn from(secret: SecretRef) -> Self {
        secret.0
    }
}

/// Чем аккаунт входит. Собирается из плоских полей `Account` (`Account::credential`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Credential {
    /// Секрет не нужен: локальный сервер или CLI без своей папки.
    None,
    /// Ключ лежит прямо в конфиге. Только наследие `[[providers]]`; `doctor` ругается.
    ApiKey(String),
    SecretRef(SecretRef),
    /// Своя папка входа CLI (`CLAUDE_CONFIG_DIR`, `XDG_DATA_HOME`, `CODEX_HOME`).
    CliHome(PathBuf),
    /// Токен подписки, выданный вне CLI. Только вручную и только не для Claude.
    DirectToken(SecretRef),
}

// ── Аккаунт ─────────────────────────────────────────────────────────────

/// Заявленные пользователем ограничения аккаунта.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountLimits {
    /// С какого процента окна аккаунт уходит вниз по списку (мягко).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_pct: Option<u8>,
    /// С какого процента аккаунт не берётся вовсе.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd_month: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
    /// Сколько вызовов одновременно. Без значения: CLI и DeepSeek — 1, API — 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
}

fn yes() -> bool {
    true
}

fn is_prompt(tools: &ToolProtocol) -> bool {
    *tools == ToolProtocol::Prompt
}

/// Одно подключение: API-ключ, DeepSeek web или официальный CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    /// Отображаемое имя; можно менять свободно.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub provider: AccountProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Модель по умолчанию.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<String>,
    #[serde(default)]
    pub limits: AccountLimits,
    /// Объявление инструментов у HTTP-аккаунта. У CLI и DeepSeek всегда промптовое.
    #[serde(default, skip_serializing_if = "is_prompt")]
    pub tools: ToolProtocol,
    /// Ссылка на секрет (`keyring:…`, `env:…`, `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretRef>,
    /// Ключ прямо в конфиге: наследие, не для новых записей.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Папка входа CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<PathBuf>,
    /// Токен подписки напрямую. Нужен `direct_token_ack = true`; для Claude запрещён.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_token: Option<SecretRef>,
    /// Владелец прочитал предупреждение о прямом токене.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub direct_token_ack: bool,
}

impl Account {
    /// Новый аккаунт без входа; остальное — по умолчанию.
    pub fn new(id: AccountId, provider: AccountProvider) -> Self {
        Self {
            id,
            label: None,
            provider,
            base_url: None,
            model: None,
            enabled: true,
            favorites: Vec::new(),
            limits: AccountLimits::default(),
            tools: ToolProtocol::default(),
            secret: None,
            api_key: None,
            home: None,
            direct_token: None,
            direct_token_ack: false,
        }
    }

    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(self.id.as_str())
    }

    /// Способ входа. Ошибка — если задано несколько способов сразу.
    pub fn credential(&self) -> Result<Credential, String> {
        let mut found: Vec<Credential> = Vec::new();
        if let Some(secret) = &self.secret
            && !secret.is_none()
        {
            found.push(Credential::SecretRef(secret.clone()));
        }
        if let Some(key) = &self.api_key {
            found.push(Credential::ApiKey(key.clone()));
        }
        if let Some(home) = &self.home {
            found.push(Credential::CliHome(home.clone()));
        }
        if let Some(token) = &self.direct_token {
            found.push(Credential::DirectToken(token.clone()));
        }
        match found.len() {
            0 => Ok(Credential::None),
            1 => Ok(found.remove(0)),
            _ => Err("only one of secret, api_key, home, direct_token may be set".to_string()),
        }
    }

    /// Как объявлять инструменты на этом аккаунте. CLI и DeepSeek — только промптом.
    pub fn tool_protocol(&self) -> ToolProtocol {
        match self.provider.protocol() {
            Some(_) => self.tools,
            None => ToolProtocol::Prompt,
        }
    }

    /// Сколько вызовов можно вести одновременно (движок держит семафор).
    pub fn max_concurrency(&self) -> u32 {
        self.limits.max_concurrency.unwrap_or(match self.provider {
            AccountProvider::DeepseekWeb
            | AccountProvider::CliClaude
            | AccountProvider::CliCodex
            | AccountProvider::CliOpencode => 1,
            _ => 4,
        })
    }

    /// Запись `[[providers]]` как аккаунт. `id` — старое имя: на нём держится `entry/model`.
    pub fn from_legacy_entry(entry: &ProviderEntry) -> Result<Self, String> {
        let mut account = Account::new(
            AccountId::parse(&entry.name)?,
            match entry.protocol {
                ProviderProtocol::Openai => AccountProvider::Openai,
                ProviderProtocol::Anthropic => AccountProvider::Anthropic,
                ProviderProtocol::Gemini => AccountProvider::Gemini,
            },
        );
        account.base_url = Some(entry.base_url.clone());
        account.model = (!entry.model.is_empty()).then(|| entry.model.clone());
        account.tools = entry.tools;
        account.api_key = entry.api_key.clone();
        Ok(account)
    }
}

// ── Маршруты и роли ─────────────────────────────────────────────────────

/// Как выбирать среди шагов. В релизе только по порядку.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Strategy {
    #[default]
    Ordered,
}

/// Что запускает переход к следующему шагу (`on = […]` в конфиге).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FallbackTrigger {
    RateLimit,
    Quota,
    SpendCap,
    Overloaded,
    Timeout,
    ServerError,
    Auth,
}

impl FallbackTrigger {
    /// Все по умолчанию, как в примере конфига.
    pub const DEFAULT: [Self; 7] = [
        Self::RateLimit,
        Self::Quota,
        Self::SpendCap,
        Self::Overloaded,
        Self::Timeout,
        Self::ServerError,
        Self::Auth,
    ];

    /// Относится ли класс отказа к этому триггеру. `BadRequest`, `ContentFilter`
    /// и `Cancelled` не относятся ни к какому: на другом аккаунте будет то же.
    pub fn matches(self, class: &FailureClass) -> bool {
        matches!(
            (self, class),
            (Self::RateLimit, FailureClass::RateLimit { .. })
                | (Self::Quota, FailureClass::QuotaExhausted { .. })
                | (Self::SpendCap, FailureClass::SpendCap)
                | (Self::Overloaded, FailureClass::Overloaded)
                | (Self::Timeout, FailureClass::Timeout | FailureClass::Network)
                | (Self::ServerError, FailureClass::Server)
                | (
                    Self::Auth,
                    FailureClass::Auth | FailureClass::AccountBlocked
                )
        )
    }
}

/// Один шаг маршрута: аккаунт и, по желанию, модель. Вложенных маршрутов нет.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub account: AccountId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Цепочка аккаунтов. Порядок шагов — и есть приоритет.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub id: RouteId,
    #[serde(default)]
    pub strategy: Strategy,
    pub steps: Vec<Step>,
    /// Пустой список значит «все» (`FallbackTrigger::DEFAULT`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on: Vec<FallbackTrigger>,
    /// Порог «заранее», процент окна. Действует только при свежем снимке лимитов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_at_pct: Option<u8>,
    /// Разрешить смесь родных и промптовых инструментов. Тогда маршрут целиком
    /// работает промптовым протоколом (`Route::tool_mode`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_mixed_tools: bool,
}

/// Как инструменты едут к модели в ходе. Фиксируется на ход: `native_tools`
/// читается один раз (`agent/runner.rs`), а смена цели не должна его менять.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    /// Поле `tools` запроса и структурные вызовы в ответе.
    Native,
    /// Описание в системном промпте, вызовы блоком `<tool_use>`.
    Prompt,
}

impl Route {
    pub fn triggers(&self) -> &[FallbackTrigger] {
        if self.on.is_empty() {
            &FallbackTrigger::DEFAULT
        } else {
            &self.on
        }
    }

    pub fn fallback_on(&self, class: &FailureClass) -> bool {
        self.triggers().iter().any(|trigger| trigger.matches(class))
    }

    /// Режим инструментов маршрута. Родной — только если все шаги родные; смесь
    /// без `allow_mixed_tools` — ошибка конфига, с ним — промптовый режим.
    pub fn tool_mode(&self, accounts: &[Account]) -> Result<ToolMode, String> {
        let mut native = 0usize;
        let mut prompt = 0usize;
        for step in &self.steps {
            let Some(account) = accounts.iter().find(|a| a.id == step.account) else {
                return Err(format!("unknown account '{}'", step.account));
            };
            match account.tool_protocol() {
                ToolProtocol::Native => native += 1,
                ToolProtocol::Prompt => prompt += 1,
            }
        }
        match (native, prompt) {
            (0, _) => Ok(ToolMode::Prompt),
            (_, 0) => Ok(ToolMode::Native),
            _ if self.allow_mixed_tools => Ok(ToolMode::Prompt),
            _ => Err(
                "route mixes native and prompt tool protocols; set allow_mixed_tools = true"
                    .to_string(),
            ),
        }
    }
}

/// Служебные вызовы, у которых может быть своя модель.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Role {
    Main,
    Title,
    Summary,
    Evaluator,
}

/// Маршрут роли: `[roles] title = "cheap"`. Пусто — роль наследует `main`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Roles(pub BTreeMap<Role, RouteId>);

impl Roles {
    /// Маршрут роли; вне `main` без своей записи роль идёт по `main`.
    pub fn route_for(&self, role: Role) -> Option<&RouteId> {
        self.0.get(&role).or_else(|| self.0.get(&Role::Main))
    }
}

// ── Корень ──────────────────────────────────────────────────────────────

/// Секция `[gateway]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewaySettings {
    #[serde(default = "one")]
    pub schema: u32,
    /// `false` — старое поведение: шлюз не вмешивается в сборку провайдера.
    #[serde(default)]
    pub enabled: bool,
}

fn one() -> u32 {
    1
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            schema: 1,
            enabled: false,
        }
    }
}

/// Все четыре таблицы шлюза. Встраивается в `Config` следующей волной.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub gateway: GatewaySettings,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub roles: Roles,
}

/// Серьёзность замечания проверки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Работать можно, пользователя предупредить.
    Warning,
    /// Конфиг нельзя применять.
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub severity: Severity,
    /// Где: `accounts[codex-work]`, `routes[coding].steps[1]`.
    pub path: String,
    pub message: String,
}

/// Текст предупреждения при включении прямого токена.
pub const DIRECT_TOKEN_WARNING: &str = "direct subscription tokens can break the provider's terms of use and get the account blocked; use the official CLI instead";

impl GatewayConfig {
    /// Старый `Config` как аккаунты: каждая `[[providers]]`, плюс `deepseek`, если есть токен.
    /// Записи с неподходящим именем пропускаются с замечанием в `problems`.
    pub fn from_legacy(config: &crate::config::Config) -> (Self, Vec<Problem>) {
        let mut out = Self::default();
        let mut problems = Vec::new();
        if !config.provider.token.is_empty() {
            let mut deepseek = Account::new(
                AccountId::parse(crate::config::BUILTIN_PROVIDER_NAME).expect("valid id"),
                AccountProvider::DeepseekWeb,
            );
            deepseek.model = Some(config.provider.model.clone());
            out.accounts.push(deepseek);
        }
        for entry in &config.providers {
            match Account::from_legacy_entry(entry) {
                Ok(account) => out.accounts.push(account),
                Err(message) => problems.push(Problem {
                    severity: Severity::Warning,
                    path: format!("providers[{}]", entry.name),
                    message,
                }),
            }
        }
        (out, problems)
    }

    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn route(&self, id: &RouteId) -> Option<&Route> {
        self.routes.iter().find(|r| &r.id == id)
    }

    /// Все замечания разом. Ошибки блокируют применение, предупреждения нет.
    pub fn validate(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        let mut add = |severity, path: String, message: String| {
            problems.push(Problem {
                severity,
                path,
                message,
            })
        };

        let mut seen: BTreeSet<&AccountId> = BTreeSet::new();
        let mut deepseek = 0;
        for account in &self.accounts {
            let at = format!("accounts[{}]", account.id);
            if !seen.insert(&account.id) {
                add(Severity::Error, at.clone(), "duplicate account id".into());
            }
            if !account.id.is_slug() {
                add(
                    Severity::Warning,
                    at.clone(),
                    "id is not a slug (a-z, 0-9, -, _); fine for old entries, avoid it in new ones"
                        .into(),
                );
            }
            if account.provider == AccountProvider::DeepseekWeb {
                deepseek += 1;
            }
            self.check_credential(account, &at, &mut add);
            if let (Some(soft), Some(hard)) = (account.limits.soft_pct, account.limits.hard_pct)
                && soft >= hard
            {
                add(
                    Severity::Error,
                    at.clone(),
                    "soft_pct must be below hard_pct".into(),
                );
            }
            for (name, pct) in [
                ("soft_pct", account.limits.soft_pct),
                ("hard_pct", account.limits.hard_pct),
            ] {
                if pct.is_some_and(|p| p > 100) {
                    add(Severity::Error, at.clone(), format!("{name} is above 100"));
                }
            }
            if account.limits.max_concurrency == Some(0) {
                add(
                    Severity::Error,
                    at.clone(),
                    "max_concurrency must be at least 1".into(),
                );
            }
            if let Some(home) = &account.home
                && account.provider.cli_tool().is_none()
            {
                add(
                    Severity::Error,
                    at.clone(),
                    format!("home ({}) is only for cli:* accounts", home.display()),
                );
            }
            if account.provider.cli_tool().is_some() && account.base_url.is_some() {
                add(
                    Severity::Error,
                    at.clone(),
                    "cli accounts have no base_url".into(),
                );
            }
            if let Some(tool) = account.provider.cli_tool() {
                if !tool.pure_model_verified() {
                    add(
                        Severity::Warning,
                        at.clone(),
                        "this CLI cannot run as a plain model yet; it will not be used for chat turns"
                            .into(),
                    );
                }
                if account.home.is_none() {
                    add(
                        Severity::Warning,
                        at.clone(),
                        "no home set: the CLI's own login is shared, a parallel session may log it out"
                            .into(),
                    );
                }
            }
        }
        if deepseek > 1 {
            add(
                Severity::Error,
                "accounts".into(),
                "only one deepseek-web account is supported (its token lives in [provider])".into(),
            );
        }

        let mut route_ids: BTreeSet<&RouteId> = BTreeSet::new();
        for route in &self.routes {
            let at = format!("routes[{}]", route.id);
            if !route_ids.insert(&route.id) {
                add(Severity::Error, at.clone(), "duplicate route id".into());
            }
            if route.steps.is_empty() {
                add(Severity::Error, at.clone(), "route has no steps".into());
            }
            for (index, step) in route.steps.iter().enumerate() {
                if self.account(&step.account).is_none() {
                    add(
                        Severity::Error,
                        format!("{at}.steps[{index}]"),
                        format!("unknown account '{}'", step.account),
                    );
                }
            }
            if route.switch_at_pct.is_some_and(|p| p > 100) {
                add(
                    Severity::Error,
                    at.clone(),
                    "switch_at_pct is above 100".into(),
                );
            }
            if route
                .steps
                .iter()
                .all(|s| self.account(&s.account).is_some())
                && let Err(message) = route.tool_mode(&self.accounts)
            {
                add(Severity::Error, at, message);
            }
        }

        for (role, route) in &self.roles.0 {
            if self.route(route).is_none() {
                add(
                    Severity::Error,
                    format!("roles.{role:?}"),
                    format!("unknown route '{route}'"),
                );
            }
        }
        problems
    }

    fn check_credential(
        &self,
        account: &Account,
        at: &str,
        add: &mut impl FnMut(Severity, String, String),
    ) {
        match account.credential() {
            Err(message) => add(Severity::Error, at.to_string(), message),
            Ok(Credential::DirectToken(_)) => {
                if account.provider.is_claude_family() {
                    add(
                        Severity::Error,
                        at.to_string(),
                        "direct tokens are not allowed for Claude: use cli:claude with the official login"
                            .into(),
                    );
                } else if !account.direct_token_ack {
                    add(
                        Severity::Error,
                        at.to_string(),
                        format!(
                            "direct_token needs direct_token_ack = true. {DIRECT_TOKEN_WARNING}"
                        ),
                    );
                } else {
                    add(
                        Severity::Warning,
                        at.to_string(),
                        DIRECT_TOKEN_WARNING.into(),
                    );
                }
            }
            Ok(Credential::ApiKey(_)) => add(
                Severity::Warning,
                at.to_string(),
                "api key stored in the config in plain text; move it to the system keyring".into(),
            ),
            Ok(Credential::CliHome(_)) | Ok(Credential::None) | Ok(Credential::SecretRef(_)) => {}
        }
        if account.direct_token_ack && account.direct_token.is_none() {
            add(
                Severity::Warning,
                at.to_string(),
                "direct_token_ack without direct_token has no effect".into(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> GatewayConfig {
        toml::from_str(toml_text).expect("config parses")
    }

    fn errors(config: &GatewayConfig) -> Vec<String> {
        config
            .validate()
            .into_iter()
            .filter(|p| p.severity == Severity::Error)
            .map(|p| format!("{}: {}", p.path, p.message))
            .collect()
    }

    const EXAMPLE: &str = r#"
        [gateway]
        schema = 1
        enabled = false

        [[accounts]]
        id = "claude-work"
        label = "Claude Code (work)"
        provider = "cli:claude"
        home = "~/.claude-work"
        secret = "none"
        favorites = ["claude-opus-5-5"]

        [[accounts]]
        id = "opencode-work"
        provider = "openai"
        base_url = "https://opencode.ai/zen/v1"
        secret = "keyring:global/opencode-work"
        model = "claude-sonnet-5-5"
        [accounts.limits]
        soft_pct = 85
        hard_pct = 97
        requests_per_minute = 30

        [[accounts]]
        id = "lmstudio"
        provider = "openai"
        base_url = "http://127.0.0.1:1234/v1"
        secret = "none"

        [[routes]]
        id = "coding"
        switch_at_pct = 90
        on = ["rate_limit", "quota", "auth"]
        steps = [
          { account = "claude-work", model = "claude-opus-5-5" },
          { account = "opencode-work" },
          { account = "lmstudio" },
        ]

        [roles]
        main = "coding"
        title = "coding"
    "#;

    #[test]
    fn the_documented_config_parses_and_validates() {
        let config = parse(EXAMPLE);
        assert!(
            !config.gateway.enabled,
            "off by default, behaviour unchanged"
        );
        assert_eq!(config.accounts.len(), 3);
        assert_eq!(config.accounts[0].provider, AccountProvider::CliClaude);
        assert_eq!(
            config.accounts[0].credential().unwrap(),
            Credential::CliHome(PathBuf::from("~/.claude-work"))
        );
        assert_eq!(config.accounts[1].limits.soft_pct, Some(85));
        assert_eq!(config.routes[0].steps[1].model, None);
        assert_eq!(errors(&config), Vec::<String>::new());
    }

    #[test]
    fn an_empty_config_is_a_disabled_gateway() {
        let config = parse("");
        assert!(!config.gateway.enabled);
        assert!(config.accounts.is_empty() && config.routes.is_empty());
        assert!(config.validate().is_empty());
    }

    #[test]
    fn a_config_round_trips_without_losing_anything() {
        let config = parse(EXAMPLE);
        let text = toml::to_string(&config).unwrap();
        assert_eq!(parse(&text), config);
    }

    #[test]
    fn ids_are_independent_of_labels_and_reject_address_breakers() {
        assert!(AccountId::parse("codex-work").unwrap().is_slug());
        assert!(
            AccountId::parse("LM Studio").is_err(),
            "space breaks addresses"
        );
        assert!(AccountId::parse("a/b").is_err());
        assert!(AccountId::parse("").is_err());
        let mut account = Account::new(
            AccountId::parse("codex-work").unwrap(),
            AccountProvider::CliCodex,
        );
        account.label = Some("Codex · работа".into());
        assert_eq!(account.display_name(), "Codex · работа");
        assert_eq!(
            account.id.as_str(),
            "codex-work",
            "renaming the label keeps the id"
        );
    }

    #[test]
    fn a_secret_that_is_not_a_reference_is_rejected_without_echoing_it() {
        let error = toml::from_str::<GatewayConfig>(
            "[[accounts]]\nid = \"a\"\nprovider = \"openai\"\nsecret = \"sk-live-abc123\"\n",
        )
        .unwrap_err();
        // Ошибка разбора TOML сама печатает строку исходника: выводить её как есть нельзя.
        assert!(error.message().contains("secret must be"), "{error}");
        assert!(!error.message().contains("abc123"), "{}", error.message());
        let own = SecretRef::parse("sk-live-abc123").unwrap_err();
        assert!(!own.contains("abc123"), "{own}");
        assert!(SecretRef::parse("keyring:").is_err(), "empty reference");
        assert!(SecretRef::parse("env:OPENAI_API_KEY").is_ok());
    }

    /// Прямой токен подписки Claude невозможен на уровне конфига.
    #[test]
    fn a_direct_token_for_claude_is_refused_even_when_acknowledged() {
        for provider in ["cli:claude", "anthropic"] {
            let config = parse(&format!(
                "[[accounts]]\nid = \"c\"\nprovider = \"{provider}\"\ndirect_token = \"keyring:c\"\ndirect_token_ack = true\n"
            ));
            let found = errors(&config);
            assert!(
                found.iter().any(|e| e.contains("not allowed for Claude")),
                "{provider}: {found:?}"
            );
        }
    }

    #[test]
    fn a_direct_token_elsewhere_needs_the_explicit_flag_and_warns() {
        let without = parse(
            "[[accounts]]\nid = \"x\"\nprovider = \"custom\"\nbase_url = \"https://h/v1\"\ndirect_token = \"keyring:x\"\n",
        );
        assert!(
            errors(&without)
                .iter()
                .any(|e| e.contains("direct_token_ack"))
        );
        let with = parse(
            "[[accounts]]\nid = \"x\"\nprovider = \"custom\"\nbase_url = \"https://h/v1\"\ndirect_token = \"keyring:x\"\ndirect_token_ack = true\n",
        );
        assert!(errors(&with).is_empty());
        assert!(
            with.validate()
                .iter()
                .any(|p| p.severity == Severity::Warning && p.message.contains("terms of use"))
        );
    }

    #[test]
    fn two_ways_to_log_in_at_once_are_an_error() {
        let config = parse(
            "[[accounts]]\nid = \"a\"\nprovider = \"openai\"\nsecret = \"env:K\"\napi_key = \"sk-x\"\n",
        );
        assert!(errors(&config).iter().any(|e| e.contains("only one of")));
    }

    #[test]
    fn broken_references_and_duplicates_are_errors() {
        let config = parse(
            r#"
            [[accounts]]
            id = "a"
            provider = "openai"
            [[accounts]]
            id = "a"
            provider = "openai"
            [[routes]]
            id = "r"
            steps = [{ account = "ghost" }]
            [[routes]]
            id = "empty"
            steps = []
            [roles]
            main = "nope"
            "#,
        );
        let found = errors(&config).join("\n");
        for needle in [
            "duplicate account id",
            "unknown account 'ghost'",
            "no steps",
            "unknown route 'nope'",
        ] {
            assert!(found.contains(needle), "{needle} not in:\n{found}");
        }
    }

    #[test]
    fn only_one_deepseek_web_account_exists() {
        let config = parse(
            "[[accounts]]\nid = \"d1\"\nprovider = \"deepseek-web\"\n[[accounts]]\nid = \"d2\"\nprovider = \"deepseek-web\"\n",
        );
        assert!(
            errors(&config)
                .iter()
                .any(|e| e.contains("only one deepseek-web"))
        );
    }

    /// Критик B1: смесь родных и промптовых инструментов в одном маршруте.
    #[test]
    fn a_route_that_mixes_tool_protocols_needs_an_explicit_opt_in() {
        let base = r#"
            [[accounts]]
            id = "api"
            provider = "openai"
            tools = "native"
            [[accounts]]
            id = "cli"
            provider = "cli:claude"
            home = "/h"
            [[routes]]
            id = "mix"
            steps = [{ account = "api" }, { account = "cli" }]
        "#;
        assert!(
            errors(&parse(base))
                .iter()
                .any(|e| e.contains("allow_mixed_tools"))
        );
        let allowed =
            parse(&base.replace("id = \"mix\"", "id = \"mix\"\n allow_mixed_tools = true"));
        assert!(errors(&allowed).is_empty());
        let mode = allowed.routes[0].tool_mode(&allowed.accounts).unwrap();
        assert_eq!(
            mode,
            ToolMode::Prompt,
            "a mixed route runs the prompt protocol for the whole turn"
        );
    }

    #[test]
    fn an_all_native_route_is_native_and_a_cli_alone_is_prompt() {
        let config = parse(
            r#"
            [[accounts]]
            id = "a"
            provider = "anthropic"
            tools = "native"
            [[accounts]]
            id = "b"
            provider = "openai"
            tools = "native"
            [[accounts]]
            id = "c"
            provider = "cli:opencode"
            home = "/h"
            [[routes]]
            id = "n"
            steps = [{ account = "a" }, { account = "b" }]
            [[routes]]
            id = "p"
            steps = [{ account = "c" }]
            "#,
        );
        assert_eq!(
            config.routes[0].tool_mode(&config.accounts),
            Ok(ToolMode::Native)
        );
        assert_eq!(
            config.routes[1].tool_mode(&config.accounts),
            Ok(ToolMode::Prompt)
        );
    }

    #[test]
    fn codex_is_warned_about_and_cli_without_home_warns_about_the_shared_login() {
        let config = parse("[[accounts]]\nid = \"cx\"\nprovider = \"cli:codex\"\n");
        let messages: Vec<String> = config.validate().into_iter().map(|p| p.message).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("cannot run as a plain model"))
        );
        assert!(messages.iter().any(|m| m.contains("may log it out")));
    }

    #[test]
    fn concurrency_defaults_keep_cli_and_deepseek_to_one_call() {
        let new = |p| Account::new(AccountId::parse("a").unwrap(), p);
        assert_eq!(new(AccountProvider::CliClaude).max_concurrency(), 1);
        assert_eq!(new(AccountProvider::DeepseekWeb).max_concurrency(), 1);
        assert_eq!(new(AccountProvider::Openai).max_concurrency(), 4);
        let mut custom = new(AccountProvider::Openai);
        custom.limits.max_concurrency = Some(0);
        let config = GatewayConfig {
            accounts: vec![custom],
            ..GatewayConfig::default()
        };
        assert!(
            errors(&config)
                .iter()
                .any(|e| e.contains("max_concurrency"))
        );
    }

    #[test]
    fn fallback_triggers_follow_the_failure_class() {
        let route = parse("[[routes]]\nid = \"r\"\nsteps = []\non = [\"rate_limit\"]\n")
            .routes
            .remove(0);
        assert!(route.fallback_on(&FailureClass::RateLimit { retry_after: None }));
        assert!(!route.fallback_on(&FailureClass::Auth));
        let everything = parse("[[routes]]\nid = \"r\"\nsteps = []\n")
            .routes
            .remove(0);
        for class in [
            FailureClass::SpendCap,
            FailureClass::QuotaExhausted { resets_at: None },
            FailureClass::Overloaded,
            FailureClass::Timeout,
            FailureClass::Server,
            FailureClass::AccountBlocked,
        ] {
            assert!(everything.fallback_on(&class), "{class:?}");
        }
        // Запрос плох сам по себе: на другом аккаунте будет то же.
        for class in [
            FailureClass::BadRequest,
            FailureClass::ContentFilter,
            FailureClass::Cancelled,
        ] {
            assert!(!everything.fallback_on(&class), "{class:?}");
        }
    }

    #[test]
    fn roles_without_their_own_route_follow_main() {
        let config = parse("[roles]\nmain = \"coding\"\ntitle = \"cheap\"\n");
        assert_eq!(
            config.roles.route_for(Role::Title).unwrap().as_str(),
            "cheap"
        );
        assert_eq!(
            config.roles.route_for(Role::Summary).unwrap().as_str(),
            "coding"
        );
        assert!(Roles::default().route_for(Role::Main).is_none());
    }

    #[test]
    fn a_legacy_config_becomes_accounts_with_the_old_names_as_ids() {
        let mut legacy = crate::config::Config::default();
        legacy.provider.token = "t".to_string();
        legacy.providers.push(ProviderEntry {
            name: "lmstudio".into(),
            base_url: "http://localhost:1234/v1".into(),
            api_key: Some("k".into()),
            model: "qwen".into(),
            protocol: ProviderProtocol::Openai,
            tools: ToolProtocol::Native,
        });
        legacy.providers.push(ProviderEntry {
            name: "My Server".into(),
            base_url: "http://x/v1".into(),
            api_key: None,
            model: String::new(),
            protocol: ProviderProtocol::Anthropic,
            tools: ToolProtocol::Prompt,
        });
        let (config, problems) = GatewayConfig::from_legacy(&legacy);
        let ids: Vec<&str> = config.accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["deepseek", "lmstudio"],
            "the name with a space is skipped"
        );
        assert_eq!(problems.len(), 1);
        let lm = &config.accounts[1];
        assert_eq!(lm.api_key.as_deref(), Some("k"));
        assert_eq!(lm.tool_protocol(), ToolProtocol::Native);
        assert!(!config.gateway.enabled);
    }

    #[test]
    fn every_cli_names_its_login_variable_and_only_codex_is_unverified() {
        assert_eq!(CliTool::Claude.home_env_var(), "CLAUDE_CONFIG_DIR");
        assert_eq!(CliTool::Opencode.home_env_var(), "XDG_DATA_HOME");
        assert!(CliTool::Claude.pure_model_verified() && CliTool::Opencode.pure_model_verified());
        assert!(!CliTool::Codex.pure_model_verified());
    }
}
