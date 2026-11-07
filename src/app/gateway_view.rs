//! Экран аккаунтов и маршрутов: состояние, кэш снимка шлюза и связка с приложением.
//! Читаем `Gateway::snapshot()`, пишем `Gateway::apply_edit()`; на диск — событием `GatewayPersist`.

pub mod edits;
#[cfg(test)]
mod flow_tests;
pub mod forms;
pub mod rows;
pub mod wizard;

use crate::app::conversation::ConversationId;
use crate::app::events::{AppEvent, Modal, View};
use crate::app::{App, switch_provider};
use crate::config::Config;
use crate::gateway::handle::{CallSource, ConfigEdit, GatewayEvent, GatewaySnapshot, LedgerQuery};
use crate::gateway::types::{Account, AccountId, GatewayConfig, Role, Severity};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

/// Как часто снимок обновляется сам (счётчики остывания идут по секундам).
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// Как часто проверяется, не изменился ли файл проектного слоя.
const PROJECT_CHECK_EVERY: Duration = Duration::from_secs(3);
/// Метка сообщения-шва: рисуется тонкой строкой, в контекст модели не идёт.
pub const SEAM_STATUS: &str = "SEAM";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Accounts,
    Routes,
    Roles,
}

impl Focus {
    pub fn next(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Accounts, true) | (Self::Roles, false) => Self::Routes,
            (Self::Routes, true) | (Self::Accounts, false) => Self::Roles,
            (Self::Roles, true) | (Self::Routes, false) => Self::Accounts,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub error: bool,
}

#[derive(Debug, Default)]
pub struct AccountsScreen {
    pub focus: Focus,
    pub selected: usize,
    pub route: usize,
    pub step: usize,
    pub role: usize,
    pub detail: bool,
    pub history: bool,
    pub notice: Option<Notice>,
    /// Ждёт ответа `y/n` на удаление.
    pub confirm_remove: Option<RemoveConfirm>,
    /// Экран «шлюз выключен»: ждёт `y/n` на включение.
    pub confirm_enable: bool,
    /// Включённый шлюз: ждёт `y/n` на выключение.
    pub confirm_disable: bool,
}

/// Вопрос об удалении аккаунта: текст называет всё, что заденет удаление.
#[derive(Debug, Clone)]
pub struct RemoveConfirm {
    pub id: AccountId,
    pub text: String,
}

/// Закрепление чата за аккаунтом (`/pin`).
#[derive(Debug, Clone)]
pub struct Pin {
    pub account: AccountId,
    pub model: Option<String>,
    /// Имя для подвала.
    pub label: String,
    /// Шлюз, для которого собран провайдер чата: пересобрали шлюз — пересобираем и пин.
    gateway: usize,
}

impl Pin {
    fn address(&self) -> String {
        match &self.model {
            Some(model) => format!("{}/{model}", self.account),
            None => self.account.to_string(),
        }
    }
}

impl AccountsScreen {
    /// Курсоры не должны выходить за списки после правок и обновлений снимка.
    pub fn clamp(&mut self, snapshot: &GatewaySnapshot) {
        self.selected = self.selected.min(snapshot.accounts.len().saturating_sub(1));
        self.route = self.route.min(snapshot.routes.len().saturating_sub(1));
        let steps = snapshot.routes.get(self.route).map_or(0, |r| r.steps.len());
        self.step = self.step.min(steps.saturating_sub(1));
        self.role = self.role.min(rows::ROLES.len() - 1);
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: false,
        });
    }

    pub fn complain(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: true,
        });
    }
}

/// За изменением файла проектного слоя следит тик: чтение метаданных идёт вне цикла событий.
#[derive(Debug, Default)]
pub struct ProjectWatch {
    /// Хэши файлов, от которых человек отказался: пока файл тот же, не спрашиваем снова.
    pub denied: std::collections::BTreeSet<String>,
    /// Подтверждён файл MCP: после перезагрузки конфига серверы поднимаются сами.
    reload_mcp: bool,
    /// Последний штамп файла: внешний `None` — ещё не смотрели, внутренний — файла нет.
    pub known: Option<Option<SystemTime>>,
    path: Option<std::path::PathBuf>,
    pub checking: bool,
    last_check: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct GatewayView {
    /// Шлюз включён (кадр не ходит к шлюзу сам).
    pub enabled: bool,
    pub snapshot: Option<GatewaySnapshot>,
    refreshed: Option<Instant>,
    pub screen: AccountsScreen,
    /// Закреплённые чаты.
    pub pins: HashMap<ConversationId, Pin>,
    pub project: ProjectWatch,
    /// Экран запущен из инструмента агента: доверие и секреты ему недоступны.
    pub restricted: bool,
}

/// Что сказать, когда экран ограничен: решение за человеком в его терминале.
pub const HUMAN_ONLY: &str = "This screen was started from an agent tool, so it can't change trust, secrets or the gateway. Run pooprusteek in your own terminal.";

impl GatewayView {
    /// Ограничен ли экран: метка агента на процессе (общая проверка `secrets::human`) или тестовый флаг.
    pub fn human_only(&self) -> bool {
        self.restricted || crate::secrets::human::is_agent_child()
    }

    /// «Сейчас» для всех подсчётов кадра: время снимка, а не часы кадра, чтобы кадр был повторяем.
    pub fn now(&self) -> SystemTime {
        self.snapshot
            .as_ref()
            .map_or_else(SystemTime::now, |s| s.generated_at)
    }

    pub fn account_rows(&self) -> Vec<rows::AccountRow> {
        self.snapshot
            .as_ref()
            .map(|s| rows::account_rows(s, self.now()))
            .unwrap_or_default()
    }
}

/// Итог мастера, когда секрет уже лёг в хранилище (или нет).
#[derive(Debug, Clone)]
pub struct AccountPlan {
    pub account: Account,
    pub login_command: Option<String>,
    pub warnings: Vec<String>,
    pub stored: Result<(), String>,
    /// Запись в хранилище, если секрет клали: на откате она удаляется.
    pub secret_id: Option<String>,
}

/// Итог мастера при выключенном шлюзе: запись `[[providers]]` записана (или нет).
#[derive(Debug, Clone)]
pub struct LegacyPlan {
    /// Ключ здесь значением (как после загрузки конфига); в файле только ссылка.
    pub entry: crate::config::ProviderEntry,
    pub stored: Result<(), String>,
}

/// Настройки движка с записью правок в конфиг: `persist` шлёт полный новый конфиг событием.
pub fn engine_deps(
    config: &Config,
    event_tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
) -> crate::gateway::engine::EngineDeps {
    struct ToApp(tokio::sync::mpsc::UnboundedSender<AppEvent>);
    impl crate::gateway::engine::ConfigPersist for ToApp {
        fn persist(&self, config: &GatewayConfig) -> Result<(), String> {
            self.0
                .send(AppEvent::GatewayPersist(Box::new(config.clone())))
                .map_err(|_| "the app is shutting down".to_string())
        }
    }
    let mut deps = crate::gateway::engine::EngineDeps::for_config(config);
    deps.persist = Some(std::sync::Arc::new(ToApp(event_tx.clone())));
    deps
}

fn humanize(code: &str) -> String {
    code.replace('_', " ")
}

/// Шов в истории: «switched a → b: rate limit, back in 2m 10s».
pub fn seam_text(from: &str, to: &str, class_code: &str, back_in: Option<&str>) -> String {
    let mut text = format!("switched {from} \u{2192} {to}: {}", humanize(class_code));
    if let Some(left) = back_in {
        text.push_str(&format!(", back in {left}"));
    }
    text
}

/// Новый `[gateway]`, аккаунты, маршруты и роли из конфига с подмешанными старыми записями.
/// `secrets` — готовые ссылки для записей с ключом; без ссылки такая запись не переносится молча.
fn assemble_enabled(
    config: &Config,
    secrets: &HashMap<String, crate::gateway::types::SecretRef>,
) -> Result<GatewayConfig, String> {
    let mut next = config.gateway_config();
    for mut account in GatewayConfig::from_legacy(config).0.accounts {
        if next.accounts.iter().any(|a| a.id == account.id) {
            continue;
        }
        if account.api_key.take().is_some() {
            let reference = secrets
                .get(account.id.as_str())
                .ok_or_else(|| format!("the key of '{}' has no place to go", account.id))?;
            account.secret = Some(reference.clone());
        }
        next.accounts.push(account);
    }
    if next.routes.is_empty()
        && let Some(route) = edits::starter_route(&next.accounts)
    {
        next.roles.0.insert(Role::Main, route.id.clone());
        next.routes.push(route);
    }
    next.gateway.enabled = true;
    if let Some(problem) = next
        .validate()
        .into_iter()
        .find(|p| p.severity == Severity::Error)
    {
        return Err(format!("{}: {}", problem.path, problem.message));
    }
    Ok(next)
}

/// Куда деть ключ записи: ссылка из файла (`env:`, `file:`, `cmd:`, любой `keyring:`) идёт в аккаунт
/// как есть; значение, лежащее прямо в файле, — в хранилище под именем записи. Блокирует.
fn carry_keys(
    path: &std::path::Path,
    keys: &[(String, String)],
    created: &mut Vec<String>,
) -> Result<HashMap<String, crate::gateway::types::SecretRef>, String> {
    use crate::gateway::types::SecretRef;
    let raw = crate::config::raw_api_keys_at(path).map_err(|error| error.to_string())?;
    let store = crate::secrets::process_store();
    let mut out = HashMap::new();
    for (name, value) in keys {
        if let Some(reference) = raw
            .get(name)
            .filter(|r| crate::secrets::looks_like_reference(r) && r.trim() != "none")
        {
            out.insert(name.clone(), SecretRef::parse(reference)?);
            continue;
        }
        let entry = crate::secrets::account_secret_id(name);
        match store.get(&entry) {
            // Стартовый перенос уже положил ключ под это имя.
            Ok(Some(_)) => {}
            Ok(None) => {
                store
                    .set_verified(&entry, &crate::secrets::Secret::new(value.as_str()))
                    .map_err(|error| format!("the key of '{name}' was not stored: {error}"))?;
                created.push(entry.clone());
            }
            Err(error) => {
                return Err(format!(
                    "the key store did not answer for '{name}': {error}"
                ));
            }
        }
        out.insert(
            name.clone(),
            SecretRef::parse(&crate::secrets::keyring_ref(&entry))?,
        );
    }
    Ok(out)
}

/// Включение целиком вне цикла событий: ключи, сборка, проверка, запись. Любой сбой откатывает всё.
fn enable_blocking(
    mut config: Config,
    path: &std::path::Path,
    keys: Vec<(String, String)>,
) -> Result<GatewayConfig, String> {
    let mut created = Vec::new();
    let outcome = carry_keys(path, &keys, &mut created)
        .and_then(|secrets| assemble_enabled(&config, &secrets))
        .and_then(|next| {
            config.gateway = next.gateway.clone();
            config.accounts = next.accounts.clone();
            config.routes = next.routes.clone();
            config.roles = next.roles.clone();
            crate::config::save_sync(&config)
                .map(|()| next)
                .map_err(|error| format!("could not save the config: {error}"))
        });
    if outcome.is_err() {
        // Ключи, положенные ради этого включения, не остаются сиротами.
        let store = crate::secrets::process_store();
        for entry in created {
            let _ = store.delete(&entry);
        }
    }
    outcome
}

/// Токен-ключ быстрой формы `<имя> [openai|anthropic|gemini] <адрес> [модель] [ключ]`: по позиции.
pub fn quick_key_token(raw: &str) -> Option<&str> {
    let mut words = raw.split_whitespace().skip(1).peekable();
    if matches!(words.peek(), Some(&("openai" | "anthropic" | "gemini"))) {
        words.next();
    }
    words.nth(2)
}

/// Текст ошибки без кусков ввода: названные слова и всё, что похоже на ключ, вычёркиваются.
pub fn echo_safe(reason: &str, hidden: &[&str]) -> String {
    let mut out = reason.to_string();
    for word in hidden.iter().filter(|w| !w.is_empty()) {
        out = out.replace(word, crate::secrets::redact::MASK);
    }
    crate::secrets::redact::redact(&out).into_owned()
}

/// Найти аккаунт по id или имени (без учёта регистра).
pub fn find_account<'a>(snapshot: &'a GatewaySnapshot, name: &str) -> Option<&'a AccountId> {
    let name = name.trim();
    snapshot
        .accounts
        .iter()
        .find(|a| a.id.as_str() == name)
        .or_else(|| {
            snapshot
                .accounts
                .iter()
                .find(|a| a.label.eq_ignore_ascii_case(name))
        })
        .map(|a| &a.id)
}

impl App {
    /// Обновить кэш снимка для кадра. Дёшево: блокировки движка, ввода-вывода нет.
    pub(crate) fn refresh_gateway_view(&mut self) {
        let view = &mut self.state.gateway_view;
        view.enabled = self.gateway.is_enabled();
        if !view.enabled {
            view.snapshot = None;
            return;
        }
        let snapshot = self.gateway.snapshot();
        view.screen.clamp(&snapshot);
        view.snapshot = Some(snapshot);
        view.refreshed = Some(Instant::now());
    }

    /// Тик: снимок раз в секунду и слежение за проектным файлом.
    pub(crate) fn tick_gateway_view(&mut self) {
        let stale = self
            .state
            .gateway_view
            .refreshed
            .is_none_or(|at| at.elapsed() >= REFRESH_EVERY);
        if stale && self.gateway.is_enabled() {
            self.refresh_gateway_view();
        }
        self.watch_project_file();
        // Диалог мог быть отложен из-за другой модалки: после её закрытия он возвращается.
        self.open_trust_dialog_if_needed();
    }

    fn project_config_path(&self) -> Option<std::path::PathBuf> {
        use crate::config::Layer;
        self.config
            .source
            .layers
            .iter()
            .find(|l| l.layer == Layer::Project)
            .map(|l| l.path.clone())
    }

    /// Файл проекта изменился — слой читается заново и, если нужно, снова спрашивает.
    fn watch_project_file(&mut self) {
        let Some(path) = self.project_config_path() else {
            return;
        };
        let watch = &mut self.state.gateway_view.project;
        // Другой проект (`/cd`): прежний штамп к нему не относится.
        if watch.path.as_ref() != Some(&path) {
            watch.path = Some(path.clone());
            watch.known = None;
        }
        if watch.checking
            || watch
                .last_check
                .is_some_and(|at| at.elapsed() < PROJECT_CHECK_EVERY)
        {
            return;
        }
        watch.checking = true;
        watch.last_check = Some(Instant::now());
        let tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let _ = tx.send(AppEvent::ProjectConfigStamp(stamp));
        });
    }

    pub(crate) fn on_project_stamp(&mut self, stamp: Option<SystemTime>) {
        let watch = &mut self.state.gateway_view.project;
        watch.checking = false;
        // Файл появился, пропал или изменился с прошлого взгляда; первый взгляд не в счёт.
        let changed = watch.known.is_some_and(|previous| previous != stamp);
        watch.known = Some(stamp);
        if changed {
            self.reload_config_async();
        }
    }

    /// Перечитать конфиг вне цикла событий; ответ придёт `ConfigReloaded`.
    pub(crate) fn reload_config_async(&mut self) {
        let tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let loaded = crate::config::load().map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::ConfigReloaded(Box::new(loaded)));
        });
    }

    pub(crate) fn on_config_reloaded(&mut self, loaded: Result<Config, String>) {
        let fresh = match loaded {
            Ok(config) => config,
            Err(error) => {
                self.state
                    .push_ui_system(&format!("Cannot reload the config: {error}"));
                return;
            }
        };
        let gateway_changed = fresh.gateway_config() != self.config.gateway_config();
        self.config = fresh;
        self.reload_mcp_after_trust();
        if gateway_changed {
            self.restart_gateway();
        }
        self.rebuild_provider();
        self.refresh_gateway_view();
        self.open_trust_dialog_if_needed();
    }

    /// Файл MCP подтверждён: серверы проекта поднимаются без перезапуска (как `/mcp reload`).
    fn reload_mcp_after_trust(&mut self) {
        let watch = &mut self.state.gateway_view.project;
        if !std::mem::take(&mut watch.reload_mcp)
            || !self
                .config
                .source
                .project_mcp
                .iter()
                .any(|file| file.trusted)
        {
            return;
        }
        self.state
            .push_ui_system("Project MCP servers are trusted: connecting them now.");
        let mcp = std::sync::Arc::clone(&self.mcp);
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            crate::mcp::manager::reload(&mcp).await;
            let _ = event_tx.send(AppEvent::McpOperationDone {
                message: "MCP servers reloaded".to_string(),
            });
        });
    }

    /// Проектный слой ждёт подтверждения — показать диалог (если ничего другого на экране нет).
    pub(crate) fn open_trust_dialog_if_needed(&mut self) {
        // Онбординг забирает клавиши раньше модалок: диалог ждёт его конца.
        if self.state.modal.is_some() || self.state.view == View::Onboarding {
            return;
        }
        // Первый запрос, от которого ещё не отказались: отказ по одному файлу не прячет остальные.
        let next = self
            .config
            .pending_trust_requests()
            .into_iter()
            .find(|request| {
                !self
                    .state
                    .gateway_view
                    .project
                    .denied
                    .contains(&request.hash)
            });
        if let Some(request) = next {
            let mut dialog = forms::TrustDialog::new(request);
            dialog.restricted = self.state.gateway_view.human_only();
            self.state.modal = Some(Modal::Trust(Box::new(dialog)));
        }
    }

    /// Решение по проектному слою. Отказ ничего не пишет: слой остаётся заблокированным.
    pub(crate) fn resolve_trust(&mut self, dialog: forms::TrustDialog) {
        if !dialog.confirmed() || self.state.gateway_view.human_only() {
            self.state
                .gateway_view
                .project
                .denied
                .insert(dialog.request.hash);
            self.state.push_ui_system(
                "Project config stays blocked: its accounts, routes, roles and MCP servers are not used.",
            );
            // Следующий файл (например, MCP после конфига) спрашивается сразу.
            self.open_trust_dialog_if_needed();
            return;
        }
        let request = dialog.request;
        let is_mcp = self
            .config
            .source
            .project_mcp
            .iter()
            .any(|file| file.request.key == request.key);
        self.state.gateway_view.project.reload_mcp = is_mcp;
        let tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let message = match crate::config::trust_project(&request) {
                Ok(()) => None,
                Err(error) => Some(format!("Cannot save the trust decision: {error}")),
            };
            match message {
                Some(message) => {
                    let _ = tx.send(AppEvent::UpdateStatus {
                        message,
                        notable: true,
                    });
                }
                None => {
                    let loaded = crate::config::load().map_err(|error| error.to_string());
                    let _ = tx.send(AppEvent::ConfigReloaded(Box::new(loaded)));
                }
            }
        });
    }

    /// Собрать шлюз заново из текущего конфига (после включения или смены слоёв).
    pub(crate) fn restart_gateway(&mut self) {
        let (gateway, pending) = crate::app::start_gateway(&self.config, &self.event_tx);
        crate::app::spawn_gateway_events(&gateway, self.event_tx.clone());
        self.gateway = gateway;
        self.pending_gateway = pending;
        self.provider_models.set_gateway(self.gateway.clone());
        // Работающий /serve переходит на новый движок (в том числе на выключенный).
        self.sync_server_gateway();
    }

    pub(crate) fn open_accounts(&mut self, focus: Focus) {
        self.state.view = View::Accounts;
        self.state.gateway_view.screen = AccountsScreen {
            focus,
            ..AccountsScreen::default()
        };
        self.refresh_gateway_view();
    }

    /// Включить шлюз: старые записи становятся аккаунтами, появляется маршрут `main`.
    /// Работа с хранилищем и файлом идёт вне цикла событий; память меняется только после успеха.
    pub(crate) fn enable_gateway(&mut self) {
        self.state.gateway_view.screen.confirm_enable = false;
        if self.state.gateway_view.human_only() {
            self.state.gateway_view.screen.complain(HUMAN_ONLY);
            return;
        }
        let known: Vec<AccountId> = self.config.accounts.iter().map(|a| a.id.clone()).collect();
        // Ключи записей, которых ещё нет аккаунтами: в памяти это уже значения, а не ссылки.
        let keys: Vec<(String, String)> = GatewayConfig::from_legacy(&self.config)
            .0
            .accounts
            .into_iter()
            .filter(|a| !known.contains(&a.id))
            .filter_map(|a| a.api_key.map(|key| (a.id.to_string(), key)))
            .collect();
        let config = self.config.clone();
        let path = self.config_target();
        let tx = self.event_tx.clone();
        self.state
            .gateway_view
            .screen
            .say("Turning the gateway on\u{2026}");
        tokio::task::spawn_blocking(move || {
            let result = enable_blocking(config, &path, keys);
            let _ = tx.send(AppEvent::GatewayEnabled(Box::new(result)));
        });
    }

    /// Шлюз включён и записан (или нет): применить в память и пересобрать.
    pub(crate) fn on_gateway_enabled(&mut self, result: Result<GatewayConfig, String>) {
        let next = match result {
            Ok(next) => next,
            Err(error) => {
                self.state.gateway_view.screen.complain(format!(
                    "The gateway was not turned on: {error}. Nothing was changed; your providers still work."
                ));
                return;
            }
        };
        self.apply_gateway_config(next);
        self.restart_gateway();
        self.rebuild_provider();
        // Экран сразу видит новое состояние (идёт сборка), а не прежний «OFF».
        self.refresh_gateway_view();
        self.state
            .gateway_view
            .screen
            .say("Gateway is on. Add accounts with a, order them with K/J.");
    }

    /// Выключить шлюз: аккаунты и маршруты остаются в файле, `[[providers]]` снова ведут.
    pub(crate) fn disable_gateway(&mut self) {
        self.state.gateway_view.screen.confirm_disable = false;
        if !self.gateway.is_enabled() {
            self.state
                .gateway_view
                .screen
                .say("The gateway is already off.");
            return;
        }
        let mut config = self.config.clone();
        config.gateway.enabled = false;
        let tx = self.event_tx.clone();
        self.state
            .gateway_view
            .screen
            .say("Turning the gateway off\u{2026}");
        tokio::task::spawn_blocking(move || {
            let result = crate::config::save_sync(&config).map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::GatewayDisabled(Box::new(result)));
        });
    }

    pub(crate) fn on_gateway_disabled(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.state.gateway_view.screen.complain(format!(
                "The gateway was not turned off: could not save the config: {error}. It stays on."
            ));
            return;
        }
        self.config.gateway.enabled = false;
        self.restart_gateway();
        // Закрепления гаснут (аккаунтов больше нет), чаты возвращаются на провайдер из `[[providers]]`.
        self.refresh_pins();
        let provider = crate::gateway::handle::main_provider(&self.config, Some(&self.gateway));
        let chats: Vec<ConversationId> = self
            .state
            .conversations
            .iter()
            .filter(|chat| chat.provider.is_some())
            .map(|chat| chat.id)
            .collect();
        for chat in chats {
            self.set_chat_provider(chat, provider.clone());
        }
        self.refresh_gateway_view();
        self.state.gateway_view.screen.selected = 0;
        self.state.gateway_view.screen.say(
            "Gateway is off. Your providers work as before; accounts and routes are kept for later.",
        );
    }

    fn apply_gateway_config(&mut self, next: GatewayConfig) {
        self.config.gateway = next.gateway;
        self.config.accounts = next.accounts;
        self.config.routes = next.routes;
        self.config.roles = next.roles;
    }

    /// Новый конфиг шлюза от движка: в память приложения и в очередь записи.
    pub(crate) fn on_gateway_persist(&mut self, next: GatewayConfig) {
        self.apply_gateway_config(next);
        if let Err(error) = crate::config::save_or_message(&self.config) {
            self.state
                .gateway_view
                .screen
                .complain(format!("Not saved: {error}"));
            self.state.status_message = error;
        }
    }

    /// Применить правку через шлюз; результат — заметка на экране.
    pub(crate) fn gateway_edit(&mut self, edit: ConfigEdit, done: &str) -> bool {
        match self.gateway.apply_edit(edit) {
            Ok(()) => {
                self.refresh_gateway_view();
                if !done.is_empty() {
                    self.state.gateway_view.screen.say(done);
                }
                true
            }
            Err(error) => {
                self.state.gateway_view.screen.complain(error.to_string());
                false
            }
        }
    }

    /// Открытая беседа без провайдера берёт новый; с другим режимом инструментов — тоже, со следующего хода.
    pub(crate) fn adopt_provider_after_edit(&mut self) {
        let focused = self.state.conversations.focused_id();
        if self.state.gateway_view.pins.contains_key(&focused) {
            return;
        }
        let fresh = crate::gateway::handle::main_provider(&self.config, Some(&self.gateway));
        let take = match (&self.state.focused().provider, &fresh) {
            (None, Some(_)) => true,
            (Some(old), Some(new)) => old.native_tools() != new.native_tools(),
            _ => false,
        };
        if take {
            switch_provider(
                &mut self.state,
                fresh,
                &self.provider_window_epoch,
                &self.event_tx,
            );
        }
    }

    /// События шлюза: обновить снимок, подобрать провайдера, показать шов.
    pub(crate) fn on_gateway_event_view(&mut self, event: &GatewayEvent) {
        match event {
            GatewayEvent::ConfigChanged => {
                self.refresh_gateway_view();
                self.refresh_pins();
                self.adopt_provider_after_edit();
            }
            GatewayEvent::FallbackTaken {
                from,
                to,
                class,
                source,
                conversation,
            } => {
                self.refresh_gateway_view();
                // Шов пишется в чат, чей вызов переключился; нет такого чата — не пишем никуда.
                if matches!(source, CallSource::Chat | CallSource::SubAgent)
                    && let Some(id) = conversation.map(ConversationId)
                    && self.state.conversations.contains(id)
                {
                    let text = self.seam_for(from, to, class.code());
                    let mut seam = crate::provider::ChatMessage::ui_system(&text);
                    seam.status = Some(SEAM_STATUS.to_string());
                    if let Some(chat) = self.state.conversations.get_mut(id) {
                        chat.messages.push(seam);
                    }
                }
            }
            _ => {
                self.state.gateway_view.refreshed = None;
            }
        }
    }

    fn seam_for(&self, from: &AccountId, to: &AccountId, code: &str) -> String {
        let snapshot = self.state.gateway_view.snapshot.as_ref();
        let label = |id: &AccountId| {
            snapshot
                .and_then(|s| s.accounts.iter().find(|a| &a.id == id))
                .map_or_else(|| id.to_string(), |a| a.label.clone())
        };
        let back = snapshot.and_then(|s| {
            let view = s.accounts.iter().find(|a| &a.id == from)?;
            let status = rows::status_text(view, self.state.gateway_view.now());
            status.text.strip_prefix("cooling ").map(str::to_string)
        });
        seam_text(&label(from), &label(to), code, back.as_deref())
    }

    /// Что мастеру известно о занятых именах; `legacy` — шлюз выключен.
    pub(crate) fn wizard_context(&self) -> (Vec<AccountId>, Vec<String>, bool) {
        let enabled = self.gateway.is_enabled();
        let snapshot = self.state.gateway_view.snapshot.as_ref();
        let mut ids: Vec<AccountId> = snapshot
            .iter()
            .flat_map(|s| s.accounts.iter().map(|a| a.id.clone()))
            .chain(self.config.accounts.iter().map(|a| a.id.clone()))
            .collect();
        let mut labels: Vec<String> = snapshot
            .iter()
            .flat_map(|s| s.accounts.iter().map(|a| a.label.clone()))
            .collect();
        for entry in &self.config.providers {
            if let Ok(id) = AccountId::parse(&entry.name) {
                ids.push(id);
            }
            labels.push(entry.name.clone());
        }
        (ids, labels, !enabled)
    }

    /// Файл, куда пишет конфиг: проектный слой не трогаем.
    fn config_target(&self) -> std::path::PathBuf {
        self.config
            .source
            .write_path
            .clone()
            .unwrap_or_else(Config::path)
    }

    fn reopen_wizard(&mut self, mut wizard: Box<wizard::AccountWizard>, error: impl Into<String>) {
        wizard.error = Some(error.into());
        self.state.modal = Some(Modal::AccountWizard(wizard));
    }

    /// Мастер дошёл до конца. Секрет пишется только когда запись точно примут: шлюз готов,
    /// кандидат прошёл проверку; при отказе после записи секрет убирается.
    pub(crate) fn submit_account_wizard(&mut self, mut wizard: Box<wizard::AccountWizard>) {
        use crate::gateway::handle::BuildState;
        if self.gateway.is_enabled() {
            match self.gateway.build_state() {
                BuildState::Ready => {}
                BuildState::Starting => {
                    return self.reopen_wizard(
                        wizard,
                        "the gateway is still starting: wait a moment and press Enter again",
                    );
                }
                BuildState::Failed(reason) => {
                    return self
                        .reopen_wizard(wizard, format!("the gateway failed to start: {reason}"));
                }
            }
        }
        // Ключ в хранилище кладёт только человек из своего терминала.
        if !wizard.secret.is_empty() && self.state.gateway_view.human_only() {
            return self.reopen_wizard(wizard, HUMAN_ONLY);
        }
        let (ids, labels, legacy) = self.wizard_context();
        let root = wizard::login_home_root();
        let existing = wizard::Existing {
            ids: &ids,
            labels: &labels,
            home_root: &root,
            legacy,
        };
        if legacy {
            return self.submit_legacy_provider(wizard, &existing);
        }
        let built = match wizard.build(&existing) {
            Ok(built) => built,
            Err(error) => return self.reopen_wizard(wizard, error),
        };
        if let Some(error) = self.new_config_errors(&built.account) {
            return self.reopen_wizard(wizard, error);
        }
        let secret_id = built.secret_id.clone();
        let secret = secret_id
            .clone()
            .map(|id| (id, wizard.secret.take_secret()));
        let tx = self.event_tx.clone();
        let plan = AccountPlan {
            account: built.account,
            login_command: built.login_command,
            warnings: built.warnings,
            stored: Ok(()),
            secret_id,
        };
        self.state.gateway_view.screen.say("Adding the account…");
        tokio::task::spawn_blocking(move || {
            let mut plan = plan;
            if let Some((id, value)) = secret {
                plan.stored = crate::secrets::process_store()
                    .set_verified(&id, &value)
                    .map_err(|error| error.to_string());
                crate::secrets::redact::register(&value);
            }
            let _ = tx.send(AppEvent::AccountStored(Box::new(plan)));
        });
    }

    /// Новая ошибка конфига, которую принесёт аккаунт; проверка до записи секрета.
    fn new_config_errors(&self, account: &Account) -> Option<String> {
        let current = self.config.gateway_config();
        let errors = |config: &GatewayConfig| -> Vec<String> {
            config
                .validate()
                .into_iter()
                .filter(|p| p.severity == Severity::Error)
                .map(|p| format!("{}: {}", p.path, p.message))
                .collect()
        };
        let known = errors(&current);
        let mut candidate = current;
        match candidate.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(existing) => *existing = account.clone(),
            None => candidate.accounts.push(account.clone()),
        }
        errors(&candidate)
            .into_iter()
            .find(|error| !known.contains(error))
    }

    /// Шлюз выключен: запись `[[providers]]` в файл ссылкой, затем ключ в хранилище; сбой ключа
    /// убирает запись.
    fn submit_legacy_provider(
        &mut self,
        mut wizard: Box<wizard::AccountWizard>,
        existing: &wizard::Existing<'_>,
    ) {
        let built = match wizard.build_legacy(existing) {
            Ok(built) => built,
            Err(error) => return self.reopen_wizard(wizard, error),
        };
        let secret = built
            .secret_id
            .clone()
            .map(|id| (id, wizard.secret.take_secret()));
        let path = self.config_target();
        let tx = self.event_tx.clone();
        let entry = built.entry;
        self.state.gateway_view.screen.say("Adding the provider…");
        tokio::task::spawn_blocking(move || {
            let plan = store_legacy_provider(&path, entry, secret);
            let _ = tx.send(AppEvent::LegacyProviderStored(Box::new(plan)));
        });
    }

    /// Запись `[[providers]]` готова (или нет): в память приложения, с подсказкой, как её включить.
    pub(crate) fn on_legacy_provider_stored(&mut self, plan: LegacyPlan) {
        if let Err(error) = &plan.stored {
            self.state
                .gateway_view
                .screen
                .complain(format!("Not added: {error}"));
            return;
        }
        let name = plan.entry.name.clone();
        self.config.providers.push(plan.entry);
        self.state.gateway_view.screen.selected = self.config.providers.len();
        self.state.gateway_view.screen.say(format!(
            "Added {name}. Enter on it makes it the active provider; the gateway stays off."
        ));
    }

    /// Секрет записан (или не нужен): добавить аккаунт и поставить его в конец главного маршрута.
    pub(crate) fn on_account_stored(&mut self, plan: AccountPlan) {
        if let Err(error) = &plan.stored {
            self.state.gateway_view.screen.complain(format!(
                "The key was not stored, so the account was not added: {error}"
            ));
            return;
        }
        let id = plan.account.id.clone();
        let label = plan.account.display_name().to_string();
        let account_copy = plan.account.clone();
        if !self.gateway_edit(ConfigEdit::UpsertAccount(Box::new(plan.account)), "") {
            // Аккаунт не принят: записанный ради него секрет сиротой не остаётся.
            if let Some(secret_id) = plan.secret_id {
                drop_secret(secret_id);
            }
            return;
        }
        let route_note = self.append_to_main_route(&account_copy);
        let mut text = format!("Added {label}.{route_note}");
        for warning in &plan.warnings {
            text.push_str(&format!(" Note: {warning}."));
        }
        if let Some(command) = plan.login_command {
            text.push_str(&format!(" Log in once: {command}"));
        }
        self.state.gateway_view.screen.say(text);
        if let Some(snapshot) = &self.state.gateway_view.snapshot {
            self.state.gateway_view.screen.selected = snapshot
                .accounts
                .iter()
                .position(|a| a.id == id)
                .unwrap_or(0);
        }
    }

    /// Новый аккаунт — последний шаг главного маршрута; маршрута нет — создаётся `main`.
    fn append_to_main_route(&mut self, added: &Account) -> String {
        let id = &added.id;
        let Some(snapshot) = self.state.gateway_view.snapshot.clone() else {
            return String::new();
        };
        match edits::main_route(&snapshot) {
            Some(route) if edits::in_route(route, id) => String::new(),
            Some(route) => {
                let Some(edit) = edits::toggle_membership(route, id) else {
                    return String::new();
                };
                if self.gateway.apply_edit(edit).is_err() {
                    // Смесь родных и промптовых инструментов: разрешаем её явно, как велит проверка.
                    let mut loosened = route.clone();
                    loosened.allow_mixed_tools = true;
                    loosened.steps.push(crate::gateway::types::Step {
                        account: id.clone(),
                        model: None,
                    });
                    if let Err(error) = self
                        .gateway
                        .apply_edit(ConfigEdit::UpsertRoute(Box::new(loosened)))
                    {
                        return format!(" It is not in the route yet: {error}.");
                    }
                }
                self.refresh_gateway_view();
                format!(" It is the last step of route {}.", route.id)
            }
            None => match edits::starter_route(std::slice::from_ref(added)) {
                Some(route) => {
                    let route_id = route.id.clone();
                    let ok = self
                        .gateway
                        .apply_edit(ConfigEdit::UpsertRoute(Box::new(route)))
                        .is_ok()
                        && self
                            .gateway
                            .apply_edit(ConfigEdit::SetRole {
                                role: Role::Main,
                                route: Some(route_id.clone()),
                            })
                            .is_ok();
                    self.refresh_gateway_view();
                    if ok {
                        format!(" Created route {route_id}.")
                    } else {
                        " It is not in a route yet.".to_string()
                    }
                }
                None => " Set its model to put it in a route.".to_string(),
            },
        }
    }

    /// Аккаунт из конфига приложения: нужен, когда правке не хватает полей, которых нет в снимке.
    pub(crate) fn account_config(&self, id: &AccountId) -> Option<Account> {
        self.config.accounts.iter().find(|a| &a.id == id).cloned()
    }

    /// `/pin <аккаунт[/модель]>` и `/pin off`: ответ показывается заметкой в чате.
    pub(crate) fn pin_chat(&mut self, arg: &str) {
        let text = match self.pin_outcome(arg) {
            Ok(text) | Err(text) => text,
        };
        self.state.push_ui_system(&text);
    }

    /// Закрепить или открепить фокусный чат. Ответ годится и для чата, и для экрана аккаунтов.
    pub(crate) fn pin_outcome(&mut self, arg: &str) -> Result<String, String> {
        let arg = arg.trim();
        let conversation = self.state.conversations.focused_id();
        if arg.is_empty() {
            return match self.state.gateway_view.pins.get(&conversation) {
                Some(pin) => Ok(format!(
                    "This chat is pinned to {}. /pin off releases it.",
                    pin.label
                )),
                None => Err("Usage: /pin <account>[/model] | /pin off".to_string()),
            };
        }
        if arg.eq_ignore_ascii_case("off") {
            if self.state.gateway_view.pins.remove(&conversation).is_none() {
                return Err("This chat is not pinned.".to_string());
            }
            self.rebuild_provider();
            return Ok("Unpinned: the chat follows the route again.".to_string());
        }
        if !self.gateway.is_enabled() {
            return Err("Turn the gateway on first (/accounts), then pin.".to_string());
        }
        let snapshot = self.gateway.snapshot();
        let (name, model) = arg.split_once('/').unwrap_or((arg, ""));
        let Some(view) = find_account(&snapshot, name)
            .and_then(|id| snapshot.accounts.iter().find(|a| &a.id == id))
        else {
            return Err(format!("No account named '{name}'. See /accounts."));
        };
        if let Some(why) = unusable_reason(&view.status) {
            return Err(format!("{} can't take a pinned chat: {why}.", view.label));
        }
        let pin = Pin {
            account: view.id.clone(),
            model: (!model.is_empty()).then(|| model.to_string()),
            label: view.label.clone(),
            gateway: gateway_key(&self.gateway),
        };
        let provider = self
            .gateway
            .provider_for_model_id(&pin.address(), CallSource::Chat)
            .map_err(|error| error.to_string())?;
        switch_provider(
            &mut self.state,
            Some(provider),
            &self.provider_window_epoch,
            &self.event_tx,
        );
        let label = pin.label.clone();
        self.state.gateway_view.pins.insert(conversation, pin);
        Ok(format!(
            "Pinned this chat to {label} until you quit or /pin off. The route is unchanged."
        ))
    }

    /// Закрепления после смены шлюза или конфига: пересобрать провайдера, а если аккаунта нет
    /// или он выключен, снять закрепление с заметкой в том чате.
    pub(crate) fn refresh_pins(&mut self) {
        use crate::gateway::handle::BuildState;
        if self.state.gateway_view.pins.is_empty()
            || self.gateway.build_state() != BuildState::Ready
        {
            return;
        }
        let snapshot = self.gateway.snapshot();
        let current = gateway_key(&self.gateway);
        let pinned: Vec<(ConversationId, Pin)> = self
            .state
            .gateway_view
            .pins
            .iter()
            .map(|(id, pin)| (*id, pin.clone()))
            .collect();
        for (conversation, pin) in pinned {
            let view = snapshot.accounts.iter().find(|a| a.id == pin.account);
            let problem = match view {
                None => Some(format!("{} was removed", pin.label)),
                Some(view) => unusable_reason(&view.status)
                    .map(|why| format!("{} can't answer: {why}", pin.label)),
            };
            if let Some(problem) = problem {
                self.state.gateway_view.pins.remove(&conversation);
                self.use_route_provider(conversation);
                self.note_in_chat(
                    conversation,
                    &format!("Unpinned: {problem}. The chat follows the route again."),
                );
                continue;
            }
            if pin.gateway == current {
                continue;
            }
            match self
                .gateway
                .provider_for_model_id(&pin.address(), CallSource::Chat)
            {
                Ok(provider) => {
                    self.set_chat_provider(conversation, Some(provider));
                    if let Some(kept) = self.state.gateway_view.pins.get_mut(&conversation) {
                        kept.gateway = current;
                    }
                }
                Err(error) => {
                    self.state.gateway_view.pins.remove(&conversation);
                    self.use_route_provider(conversation);
                    self.note_in_chat(
                        conversation,
                        &format!("Unpinned: {} is not available ({error}).", pin.label),
                    );
                }
            }
        }
    }

    /// Положить провайдера в чат; у фокусного заодно меняется сессия и окно контекста.
    fn set_chat_provider(
        &mut self,
        conversation: ConversationId,
        provider: Option<std::sync::Arc<dyn crate::provider::LLMProvider>>,
    ) {
        if conversation == self.state.conversations.focused_id() {
            switch_provider(
                &mut self.state,
                provider,
                &self.provider_window_epoch,
                &self.event_tx,
            );
        } else if let Some(chat) = self.state.conversations.get_mut(conversation) {
            chat.provider = provider;
            chat.session_id = crate::session::create_session_id();
        }
    }

    /// Чат снова идёт по маршруту (пин снят).
    fn use_route_provider(&mut self, conversation: ConversationId) {
        let provider = crate::gateway::handle::main_provider(&self.config, Some(&self.gateway));
        self.set_chat_provider(conversation, provider);
    }

    /// Заметка в нужном чате, а не в том, что на экране.
    fn note_in_chat(&mut self, conversation: ConversationId, text: &str) {
        if conversation == self.state.conversations.focused_id() {
            self.state.push_ui_system(text);
        } else if let Some(chat) = self.state.conversations.get_mut(conversation) {
            chat.messages
                .push(crate::provider::ChatMessage::ui_system(text));
        }
    }

    /// Снять закрепления за аккаунтом (его удалили) и сказать об этом в каждом чате.
    pub(crate) fn unpin_account(&mut self, account: &AccountId, label: &str) -> usize {
        let chats: Vec<ConversationId> = self
            .state
            .gateway_view
            .pins
            .iter()
            .filter(|(_, pin)| &pin.account == account)
            .map(|(id, _)| *id)
            .collect();
        for conversation in &chats {
            self.state.gateway_view.pins.remove(conversation);
            self.use_route_provider(*conversation);
            self.note_in_chat(
                *conversation,
                &format!("Unpinned: {label} was removed. The chat follows the route again."),
            );
        }
        chats.len()
    }

    /// Вопрос об удалении аккаунта: называет маршруты, закреплённые чаты и ключ в хранилище.
    /// Единственный шаг маршрута не снимаем: отказ с объяснением.
    pub(crate) fn ask_remove_account(&mut self, id: &AccountId) {
        let Some(snapshot) = self.state.gateway_view.snapshot.clone() else {
            return;
        };
        let label = snapshot
            .accounts
            .iter()
            .find(|a| &a.id == id)
            .map_or_else(|| id.to_string(), |a| a.label.clone());
        let route_name = |route: &crate::gateway::types::Route| {
            let roles: Vec<String> = snapshot
                .roles
                .0
                .iter()
                .filter(|(_, target)| **target == route.id)
                .map(|(role, _)| rows::role_name(*role).to_string())
                .collect();
            if roles.is_empty() {
                route.id.to_string()
            } else {
                format!("{} (role {})", route.id, roles.join(", "))
            }
        };
        let routes: Vec<&crate::gateway::types::Route> = snapshot
            .routes
            .iter()
            .filter(|r| edits::in_route(r, id))
            .collect();
        if let Some(only) = routes
            .iter()
            .find(|r| r.steps.iter().all(|s| &s.account == id))
        {
            self.state.gateway_view.screen.complain(format!(
                "Can't remove {label}: it is the only step of route {}. Add another account to that route first.",
                route_name(only)
            ));
            return;
        }
        let mut text = format!("Remove {label}?");
        if !routes.is_empty() {
            let names: Vec<String> = routes.iter().map(|r| route_name(r)).collect();
            text.push_str(&format!(" Its steps in route {} go too.", names.join(", ")));
        }
        let pinned = self
            .state
            .gateway_view
            .pins
            .values()
            .filter(|pin| &pin.account == id)
            .count();
        if pinned > 0 {
            text.push_str(&format!(" {pinned} pinned chat(s) go back to the route."));
        }
        if let Some(entry) = self.stored_key_of(id) {
            text.push_str(&format!(
                " The key stays in the keyring; /secrets rm {entry} deletes it."
            ));
        }
        text.push_str(" y/n");
        self.state.gateway_view.screen.confirm_remove = Some(RemoveConfirm {
            id: id.clone(),
            text,
        });
    }

    /// Имя записи в хранилище, если ключ аккаунта лежит там.
    fn stored_key_of(&self, id: &AccountId) -> Option<String> {
        let account = self.account_config(id)?;
        [&account.secret, &account.direct_token]
            .into_iter()
            .flatten()
            .find_map(|reference| reference.as_str().strip_prefix("keyring:"))
            .map(str::to_string)
    }

    /// Подтверждённое удаление: шаги маршрутов уходят той же правкой, закрепления снимаются.
    pub(crate) fn remove_account(&mut self, id: &AccountId) {
        let label = self
            .state
            .gateway_view
            .snapshot
            .as_ref()
            .and_then(|s| s.accounts.iter().find(|a| &a.id == id))
            .map_or_else(|| id.to_string(), |a| a.label.clone());
        let key = self.stored_key_of(id);
        if !self.gateway_edit(ConfigEdit::RemoveAccount(id.clone()), "") {
            return;
        }
        let unpinned = self.unpin_account(id, &label);
        let mut text = format!("Removed {label}.");
        if unpinned > 0 {
            text.push_str(&format!(
                " {unpinned} pinned chat(s) follow the route again."
            ));
        }
        if let Some(key) = key {
            text.push_str(&format!(
                " Its key stays in the keyring (/secrets rm {key})."
            ));
        }
        self.state.gateway_view.screen.say(text);
    }

    /// `/usage`: расход за сегодня и вчера (учёт посуточный, по UTC) по аккаунтам.
    pub(crate) fn show_usage(&mut self) {
        if !self.gateway.is_enabled() {
            self.state
                .push_ui_system("Usage is counted once the gateway is on (/accounts).");
            return;
        }
        let since = SystemTime::now().checked_sub(Duration::from_secs(86_400));
        let totals = self.gateway.ledger().totals(&LedgerQuery {
            account: None,
            since,
        });
        let snapshot = self.gateway.snapshot();
        let labels: HashMap<String, String> = snapshot
            .accounts
            .iter()
            .map(|a| (a.id.to_string(), a.label.clone()))
            .collect();
        let text = usage_table(&totals, &labels);
        self.state.push_ui_system(&text);
    }

    /// Шлюз выключен: объявление инструментов записи `[[providers]]` меняется в файле.
    fn set_legacy_tools(&mut self, name: &str, tools: crate::config::ToolProtocol) {
        let Some(entry) = self.config.providers.iter().find(|e| e.name == name) else {
            self.state
                .push_ui_system(&format!("No provider named '{name}' - see /accounts."));
            return;
        };
        let word = match tools {
            crate::config::ToolProtocol::Native => "native",
            crate::config::ToolProtocol::Prompt => "prompt",
        };
        let entry_name = entry.name.clone();
        match crate::config::set_provider_field_at(
            &self.config_target(),
            &entry_name,
            "tools",
            word,
        ) {
            Ok(()) => {
                if let Some(entry) = self
                    .config
                    .providers
                    .iter_mut()
                    .find(|e| e.name == entry_name)
                {
                    entry.tools = tools;
                }
                self.rebuild_provider();
                self.state.push_ui_system(&format!(
                    "Provider '{entry_name}' now declares tools: {tools:?}."
                ));
            }
            Err(error) => self
                .state
                .push_ui_system(&format!("Failed to save config: {error}")),
        }
    }

    /// `/providers tools <имя> <native|prompt>` для аккаунта.
    pub(crate) fn set_account_tools(&mut self, name: &str, tools: crate::config::ToolProtocol) {
        let Some(snapshot) = self
            .state
            .gateway_view
            .snapshot
            .clone()
            .or_else(|| self.gateway.is_enabled().then(|| self.gateway.snapshot()))
        else {
            self.set_legacy_tools(name, tools);
            return;
        };
        let Some(id) = find_account(&snapshot, name).cloned() else {
            self.state
                .push_ui_system(&format!("No account named '{name}' — see /accounts."));
            return;
        };
        let Some(mut account) = self.account_config(&id) else {
            return;
        };
        if account.provider.protocol().is_none() {
            self.state.push_ui_system(
                "This account has no tool-calling channel: its tools are always described in the prompt.",
            );
            return;
        }
        account.tools = tools;
        if self.gateway_edit(ConfigEdit::UpsertAccount(Box::new(account)), "") {
            self.state
                .push_ui_system(&format!("Account '{name}' now declares tools: {tools:?}."));
        } else if let Some(notice) = &self.state.gateway_view.screen.notice {
            let text = notice.text.clone();
            self.state.push_ui_system(&text);
        }
    }
}

/// Идентичность собранного шлюза: пока она та же, провайдер чата действителен.
fn gateway_key(gateway: &crate::gateway::handle::GatewayHandle) -> usize {
    std::sync::Arc::as_ptr(gateway).cast::<()>() as usize
}

/// Почему аккаунт не может отвечать чату; `None` — может (остывание и лимит проходят).
fn unusable_reason(status: &crate::gateway::handle::AccountStatus) -> Option<String> {
    use crate::gateway::handle::AccountStatus;
    match status {
        AccountStatus::Disabled => Some("it is switched off (Space turns it on)".to_string()),
        AccountStatus::Unavailable { reason } | AccountStatus::NeedsAttention { reason } => {
            Some(reason.clone())
        }
        _ => None,
    }
}

/// Записать `[[providers]]` ссылкой на ключ, затем сам ключ. Блокирующий; зовётся из `spawn_blocking`.
fn store_legacy_provider(
    path: &std::path::Path,
    mut entry: crate::config::ProviderEntry,
    secret: Option<(String, crate::secrets::Secret)>,
) -> LegacyPlan {
    let reference = secret
        .as_ref()
        .map(|(id, _)| crate::secrets::keyring_ref(id));
    if let Err(error) = crate::config::add_provider_at(path, &entry, reference.as_deref()) {
        return LegacyPlan {
            entry,
            stored: Err(error.to_string()),
        };
    }
    if let Some((id, value)) = secret {
        crate::secrets::redact::register(&value);
        if let Err(error) = crate::secrets::process_store().set_verified(&id, &value) {
            // Ключ не лёг: запись без ключа сиротой не оставляем.
            let _ = crate::config::remove_provider_at(path, &entry.name);
            return LegacyPlan {
                entry,
                stored: Err(format!(
                    "the key was not stored, nothing was added: {error}"
                )),
            };
        }
        entry.api_key = Some(value.expose().to_string());
    }
    LegacyPlan {
        entry,
        stored: Ok(()),
    }
}

/// Убрать запись из хранилища (откат); отказ хранилища не страшен, остаётся строка в журнале.
fn drop_secret(id: String) {
    tokio::task::spawn_blocking(move || {
        if let Err(error) = crate::secrets::process_store().delete(&id) {
            tracing::warn!("secret {id} was not removed after a refused account: {error}");
        }
    });
}

/// Компактное число токенов: `1.2M`, `96k`.
fn tokens_text(value: u64) -> String {
    match value {
        0..1_000 => value.to_string(),
        1_000..1_000_000 => format!("{}k", value / 1_000),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

/// Таблица расхода по аккаунтам.
pub fn usage_table(
    rows: &[crate::gateway::handle::LedgerRow],
    labels: &HashMap<String, String>,
) -> String {
    #[derive(Default)]
    struct Sum {
        calls: u64,
        failed: u64,
        fallbacks: u64,
        prompt: u64,
        completion: u64,
        cache: u64,
        estimated: bool,
    }
    let mut sums: Vec<(String, Sum)> = Vec::new();
    for row in rows {
        let name = labels
            .get(row.account.as_str())
            .cloned()
            .unwrap_or_else(|| row.account.to_string());
        let at = match sums.iter().position(|(n, _)| *n == name) {
            Some(at) => at,
            None => {
                sums.push((name, Sum::default()));
                sums.len() - 1
            }
        };
        let sum = &mut sums[at].1;
        sum.calls += row.calls;
        sum.failed += row.failed;
        sum.fallbacks += row.fallbacks;
        sum.prompt += row.prompt_tokens;
        sum.completion += row.completion_tokens;
        sum.cache += row.cache_read_tokens;
        sum.estimated |= row.any_estimated;
    }
    if sums.is_empty() {
        return "Usage, today and yesterday (UTC days): no calls yet.".to_string();
    }
    let width = sums
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(7)
        .max(7);
    let mut out = format!(
        "Usage, today and yesterday (UTC days)\n{:<width$}  {:>6} {:>6} {:>9} {:>8} {:>8} {:>6}",
        "account", "calls", "failed", "fallbacks", "in", "out", "cache"
    );
    for (name, sum) in &sums {
        let cache = (sum.cache * 100)
            .checked_div(sum.prompt)
            .map_or_else(|| "-".to_string(), |share| format!("{share}%"));
        out.push_str(&format!(
            "\n{:<width$}  {:>6} {:>6} {:>9} {:>8} {:>8} {:>6}{}",
            name,
            sum.calls,
            sum.failed,
            sum.fallbacks,
            tokens_text(sum.prompt),
            tokens_text(sum.completion),
            cache,
            if sum.estimated { "  ~ estimated" } else { "" },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::handle::{CallSource, Gateway, LedgerRow};
    use crate::gateway::testkit::{TWO_ACCOUNTS, rig};

    #[test]
    fn a_seam_names_both_accounts_and_the_reason() {
        let text = seam_text("claude-max", "opencode-go", "rate_limit", Some("2m 10s"));
        assert_eq!(
            text,
            "switched claude-max \u{2192} opencode-go: rate limit, back in 2m 10s"
        );
        assert_eq!(
            seam_text("a", "b", "server_error", None),
            "switched a \u{2192} b: server error"
        );
    }

    #[test]
    fn accounts_are_found_by_id_or_by_the_name_the_user_gave() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        assert_eq!(find_account(&snapshot, "a").map(|i| i.as_str()), Some("a"));
        assert!(find_account(&snapshot, "zzz").is_none());
        rig.engine
            .apply_edit(ConfigEdit::SetLabel {
                id: AccountId::parse("b").unwrap(),
                label: "Work Claude".into(),
            })
            .unwrap();
        let snapshot = rig.engine.snapshot();
        assert_eq!(
            find_account(&snapshot, "work claude").map(|i| i.as_str()),
            Some("b"),
            "the label works as a name, case-insensitively"
        );
        // Переименование не меняет id.
        assert_eq!(snapshot.accounts[1].id.as_str(), "b");
        assert_eq!(snapshot.accounts[1].label, "Work Claude");
    }

    #[test]
    fn the_usage_table_sums_models_and_marks_estimates() {
        let row = |account: &str, model: &str, calls: u64, est: bool| LedgerRow {
            account: AccountId::parse(account).unwrap(),
            model: model.into(),
            source: CallSource::Chat,
            calls,
            failed: 1,
            fallbacks: 0,
            prompt_tokens: 2_000,
            completion_tokens: 500,
            cache_read_tokens: 1_000,
            any_estimated: est,
        };
        let mut labels = HashMap::new();
        labels.insert("a".to_string(), "Claude Max".to_string());
        let table = usage_table(
            &[row("a", "m1", 3, false), row("a", "m2", 4, true)],
            &labels,
        );
        assert!(table.contains("Claude Max"), "{table}");
        assert!(table.contains("      7"), "calls are summed: {table}");
        assert!(table.contains("50%"), "cache share: {table}");
        assert!(table.contains("~ estimated"), "{table}");
        assert!(usage_table(&[], &labels).contains("no calls yet"));
    }

    #[test]
    fn the_cursors_stay_inside_the_lists_after_a_removal() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let snapshot = rig.engine.snapshot();
        let mut screen = AccountsScreen {
            selected: 7,
            step: 9,
            route: 4,
            ..AccountsScreen::default()
        };
        screen.clamp(&snapshot);
        assert_eq!(screen.selected, 1);
        assert_eq!(screen.route, 0);
        assert_eq!(screen.step, 1);
    }

    #[test]
    fn focus_cycles_through_the_three_blocks_both_ways() {
        assert_eq!(Focus::Accounts.next(true), Focus::Routes);
        assert_eq!(Focus::Roles.next(true), Focus::Accounts);
        assert_eq!(Focus::Accounts.next(false), Focus::Roles);
    }

    // ── Уровень приложения: настоящий `App`, движок на заглушках ─────────

    use crate::app::events::AppEvent;
    use crate::gateway::testkit::{Act, FakeBackend, ask};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    const NO_ACCOUNTS: &str = "[gateway]\nenabled = true\n";

    async fn test_app() -> App {
        let mut config = Config::default();
        config.semantic.enabled = false;
        config.update.auto = false;
        App::new(config).await.expect("the app starts")
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press_in_wizard(app: &mut App, code: KeyCode) {
        let Some(Modal::AccountWizard(wizard)) = app.state.modal.take() else {
            panic!("the wizard is closed");
        };
        app.handle_account_wizard_key(press(code), wizard)
            .await
            .unwrap();
    }

    async fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            press_in_wizard(app, KeyCode::Char(ch)).await;
        }
    }

    /// Один проход мастера для Claude Code: тип, имя, своя папка входа, подтверждение.
    async fn add_claude(app: &mut App, name: &str) {
        app.state.modal = Some(Modal::AccountWizard(Box::default()));
        press_in_wizard(app, KeyCode::Down).await; // Claude Code
        press_in_wizard(app, KeyCode::Enter).await;
        type_text(app, name).await;
        press_in_wizard(app, KeyCode::Enter).await;
        press_in_wizard(app, KeyCode::Enter).await; // own login folder
        press_in_wizard(app, KeyCode::Enter).await; // confirm
        let event = tokio::time::timeout(Duration::from_secs(60), app.event_rx.recv())
            .await
            .expect("the plan arrives")
            .expect("channel open");
        let AppEvent::AccountStored(plan) = event else {
            panic!("expected AccountStored, got {event:?}");
        };
        app.on_account_stored(*plan);
    }

    #[tokio::test]
    async fn two_subscriptions_of_one_kind_are_added_with_a_handful_of_keys() {
        let rig = rig(NO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.refresh_gateway_view();

        add_claude(&mut app, "claude-max").await;
        add_claude(&mut app, "claude-work").await;

        let snapshot = app.gateway.snapshot();
        let labels: Vec<_> = snapshot.accounts.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["claude-max", "claude-work"], "the user's names");
        assert!(
            snapshot
                .problems
                .iter()
                .all(|p| p.severity != Severity::Error),
            "no config errors: {:?}",
            snapshot.problems
        );
        let route = edits::main_route(&snapshot).expect("a route was created");
        assert_eq!(route.steps.len(), 2, "both are steps of the main route");
        let notice = app
            .state
            .gateway_view
            .screen
            .notice
            .clone()
            .expect("a notice");
        assert!(notice.text.contains("Log in once"), "{}", notice.text);
        assert!(
            notice.text.contains("claude-work"),
            "an own login folder per account: {}",
            notice.text
        );
    }

    #[tokio::test]
    async fn a_duplicate_name_is_refused_in_the_wizard() {
        let rig = rig(NO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.refresh_gateway_view();
        add_claude(&mut app, "same").await;
        app.state.modal = Some(Modal::AccountWizard(Box::default()));
        press_in_wizard(&mut app, KeyCode::Down).await;
        press_in_wizard(&mut app, KeyCode::Enter).await;
        type_text(&mut app, "SAME").await;
        press_in_wizard(&mut app, KeyCode::Enter).await;
        let Some(Modal::AccountWizard(wizard)) = &app.state.modal else {
            panic!("the wizard stays open");
        };
        assert!(
            wizard
                .error
                .as_deref()
                .unwrap_or("")
                .contains("already used")
        );
    }

    #[tokio::test]
    async fn a_pinned_chat_stays_on_its_account_and_unpin_returns_to_the_route() {
        let rig = rig(
            TWO_ACCOUNTS,
            vec![
                (
                    "a",
                    FakeBackend::new(vec![Act::Reply("from a"), Act::Reply("from a")]),
                ),
                (
                    "b",
                    FakeBackend::new(vec![Act::Reply("from b"), Act::Reply("from b")]),
                ),
            ],
        );
        let (fake_a, fake_b) = (rig.fakes["a"].clone(), rig.fakes["b"].clone());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        // Конфиг согласован с движком, иначе `/pin off` посчитает шлюз устаревшим.
        app.config.gateway.enabled = true;
        app.refresh_gateway_view();

        app.pin_chat("b");
        let pinned = app.state.focused().provider.clone().expect("a provider");
        let (result, text) = ask(&pinned, "hi").await;
        result.unwrap();
        assert_eq!(
            text, "from b",
            "the route puts a first, the pin overrides it"
        );
        assert_eq!(fake_a.call_count(), 0);
        let focused = app.state.conversations.focused_id();
        assert!(app.state.gateway_view.pins.contains_key(&focused));

        app.pin_chat("off");
        let free = app.state.focused().provider.clone().expect("a provider");
        let (_, text) = ask(&free, "hi").await;
        assert_eq!(text, "from a", "unpinned, the route decides again");
        assert_eq!(fake_b.call_count(), 1);
    }

    #[tokio::test]
    async fn a_failed_model_refresh_keeps_the_welcome_screen_but_warns_in_a_chat() {
        let mut app = test_app().await;
        app.on_provider_models_refreshed("provider models: mock failed".to_string(), 1);
        assert!(app.state.focused().messages.is_empty());
        assert_eq!(app.state.status_message, "provider models: mock failed");

        app.state.push_ui_system("earlier note");
        app.on_provider_models_refreshed("provider models: mock failed".to_string(), 1);
        assert_eq!(app.state.focused().messages.len(), 2);
    }

    #[tokio::test]
    async fn pinning_an_unknown_account_says_so() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.pin_chat("nobody");
        let last = app
            .state
            .focused()
            .messages
            .last()
            .expect("a note")
            .content
            .clone();
        assert!(last.contains("No account named"), "{last}");
        assert!(app.state.gateway_view.pins.is_empty());
    }

    #[tokio::test]
    async fn a_fallback_leaves_a_seam_in_the_history_that_the_model_never_sees() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.refresh_gateway_view();
        let chat = Some(app.state.conversations.focused_id().0);
        let event = |source| GatewayEvent::FallbackTaken {
            from: AccountId::parse("a").unwrap(),
            to: AccountId::parse("b").unwrap(),
            class: crate::provider::failure::FailureClass::RateLimit { retry_after: None },
            source,
            conversation: chat,
        };
        app.on_gateway_event_view(&event(CallSource::Chat));
        let seam = app.state.focused().messages.last().expect("a seam").clone();
        assert!(seam.ui_only, "a seam is UI only");
        assert_eq!(seam.status.as_deref(), Some(SEAM_STATUS));
        assert!(
            seam.content.contains("switched a \u{2192} b: rate limit"),
            "{}",
            seam.content
        );
        // Служебные вызовы (роли, /serve) шва в чате не оставляют.
        let before = app.state.focused().messages.len();
        app.on_gateway_event_view(&event(CallSource::Serve));
        assert_eq!(app.state.focused().messages.len(), before);
    }

    fn fallback_in(conversation: Option<u64>) -> GatewayEvent {
        GatewayEvent::FallbackTaken {
            from: AccountId::parse("a").unwrap(),
            to: AccountId::parse("b").unwrap(),
            class: crate::provider::failure::FailureClass::RateLimit { retry_after: None },
            source: CallSource::Chat,
            conversation,
        }
    }

    #[tokio::test]
    async fn a_seam_lands_in_the_chat_that_switched_not_in_the_focused_one() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.refresh_gateway_view();
        let background = crate::app::conversation::Conversation::fresh_main(None);
        let background_id = background.id;
        app.state.conversations.add_background(background);
        let focused_before = app.state.focused().messages.len();

        app.on_gateway_event_view(&fallback_in(Some(background_id.0)));

        assert_eq!(
            app.state.focused().messages.len(),
            focused_before,
            "the chat on screen did not switch, so it gets no seam"
        );
        let other = app.state.conversations.get(background_id).unwrap();
        assert!(
            other
                .messages
                .last()
                .is_some_and(|m| m.content.contains("switched a")),
            "the chat that switched has the seam"
        );
    }

    #[tokio::test]
    async fn a_fallback_with_no_chat_writes_no_seam_anywhere() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.refresh_gateway_view();
        app.on_gateway_event_view(&fallback_in(None));
        app.on_gateway_event_view(&fallback_in(Some(u64::MAX)));
        assert!(app.state.focused().messages.is_empty());
    }

    #[tokio::test]
    async fn a_config_change_gives_a_chat_without_a_provider_the_new_one() {
        let rig = rig(TWO_ACCOUNTS, Vec::new());
        let mut app = test_app().await;
        app.gateway = Arc::new(rig.engine);
        app.state.focused_mut().provider = None;
        app.on_gateway_event_view(&GatewayEvent::ConfigChanged);
        assert!(app.state.focused().provider.is_some());
    }

    #[tokio::test]
    async fn an_edit_is_handed_to_the_config_queue_by_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = Config::default();
        config.semantic.enabled = false;
        config.gateway.enabled = true;
        for name in ["a", "b"] {
            let mut account = Account::new(
                AccountId::parse(name).unwrap(),
                crate::gateway::types::AccountProvider::Custom,
            );
            account.base_url = Some(format!("http://{name}.invalid/v1"));
            account.model = Some("m".into());
            account.secret = Some(crate::gateway::types::SecretRef::parse("none").unwrap());
            config.accounts.push(account);
        }
        let engine = crate::gateway::engine::build_gateway(
            config.gateway_config(),
            engine_deps(&config, &tx),
        );
        engine
            .apply_edit(ConfigEdit::MoveAccount {
                id: AccountId::parse("a").unwrap(),
                to_index: 1,
            })
            .unwrap();
        let AppEvent::GatewayPersist(next) = rx.try_recv().expect("the new config was handed over")
        else {
            panic!("expected GatewayPersist");
        };
        let order: Vec<_> = next.accounts.iter().map(|a| a.id.to_string()).collect();
        assert_eq!(order, ["b", "a"]);
        // Приложение кладёт его в свой конфиг (и в очередь записи).
        let mut app = test_app().await;
        app.on_gateway_persist(*next);
        assert_eq!(app.config.accounts[0].id.as_str(), "b");
    }

    #[tokio::test]
    async fn the_trust_dialog_opens_on_deny_and_enter_keeps_the_layer_blocked() {
        let mut app = test_app().await;
        app.state.view = View::Chat;
        app.config.source.pending_trust = Some(crate::config::TrustRequest {
            config_path: "x/.pooprusteek/config.toml".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back: vec!["account evil (https://evil.test)".into()],
            summary: Default::default(),
        });
        app.open_trust_dialog_if_needed();
        let Some(Modal::Trust(dialog)) = app.state.modal.take() else {
            panic!("the dialog is open");
        };
        assert!(!dialog.confirmed(), "focus starts on the refusal");
        app.handle_trust_key(press(KeyCode::Enter), dialog)
            .await
            .unwrap();
        assert!(app.state.modal.is_none());
        let note = app
            .state
            .focused()
            .messages
            .last()
            .expect("a note")
            .content
            .clone();
        assert!(note.contains("stays blocked"), "{note}");
        assert!(app.config.trust.projects.is_empty(), "nothing was trusted");
    }

    #[tokio::test]
    async fn a_refusal_is_not_asked_again_until_the_file_changes() {
        let mut app = test_app().await;
        app.state.view = View::Chat;
        let request = |hash: &str| crate::config::TrustRequest {
            config_path: "p".into(),
            key: "k".into(),
            hash: hash.into(),
            held_back: Vec::new(),
            summary: Default::default(),
        };
        app.config.source.pending_trust = Some(request("one"));
        app.open_trust_dialog_if_needed();
        let Some(Modal::Trust(dialog)) = app.state.modal.take() else {
            panic!("asked the first time");
        };
        app.handle_trust_key(press(KeyCode::Esc), dialog)
            .await
            .unwrap();
        app.open_trust_dialog_if_needed();
        assert!(app.state.modal.is_none(), "same file, no second question");
        app.config.source.pending_trust = Some(request("two"));
        app.open_trust_dialog_if_needed();
        assert!(app.state.modal.is_some(), "the file changed: ask again");
    }

    #[tokio::test]
    async fn y_does_not_trust_and_escape_denies() {
        let mut app = test_app().await;
        let request = crate::config::TrustRequest {
            config_path: "p".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back: Vec::new(),
            summary: Default::default(),
        };
        let dialog = Box::new(forms::TrustDialog::new(request));
        app.handle_trust_key(press(KeyCode::Char('y')), dialog.clone())
            .await
            .unwrap();
        assert!(
            matches!(&app.state.modal, Some(Modal::Trust(d)) if !d.confirmed()),
            "y neither trusts nor closes"
        );
        app.state.modal = None;
        app.handle_trust_key(press(KeyCode::Esc), dialog)
            .await
            .unwrap();
        assert!(app.state.modal.is_none());
    }

    #[tokio::test]
    async fn a_typed_secret_never_reaches_the_input_the_history_or_the_screen() {
        let mut app = test_app().await;
        app.state.modal = Some(Modal::SecretPrompt(Box::new(forms::SecretPrompt::named(
            "openai",
        ))));
        for ch in "sk-live-1234567890".chars() {
            let Some(Modal::SecretPrompt(prompt)) = app.state.modal.take() else {
                panic!("the prompt is open");
            };
            app.handle_secret_prompt_key(press(KeyCode::Char(ch)), prompt)
                .await
                .unwrap();
        }
        assert!(app.state.input.buffer.is_empty(), "not in the input field");
        assert!(
            !app.state
                .input
                .history
                .iter()
                .any(|h| h.contains("sk-live")),
            "not in the history"
        );
        let Some(modal) = &app.state.modal else {
            panic!("the prompt stays open");
        };
        assert!(
            !format!("{modal:?}").contains("sk-live"),
            "not in Debug output"
        );
        // Кадр: значение скрыто маской.
        let config = Config::default();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                crate::tui::render::draw_frame(frame, &app.state, &config);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                screen.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(
            !screen.contains("sk-live") && !screen.contains("1234567890"),
            "{screen}"
        );
        assert!(
            screen.contains('\u{2022}') || screen.contains('*'),
            "a mask is drawn"
        );
    }

    #[tokio::test]
    async fn pasting_a_secret_goes_to_the_hidden_field_not_the_prompt_line() {
        let mut app = test_app().await;
        app.state.modal = Some(Modal::SecretPrompt(Box::new(forms::SecretPrompt::named(
            "k",
        ))));
        app.state.view = View::Chat;
        app.handle_paste("pasted-secret-value".to_string());
        assert!(app.state.input.buffer.is_empty());
        let Some(Modal::SecretPrompt(prompt)) = &app.state.modal else {
            panic!("open");
        };
        assert_eq!(prompt.value.len(), "pasted-secret-value".len());
    }
}
