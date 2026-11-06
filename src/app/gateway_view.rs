//! Экран аккаунтов и маршрутов: состояние, кэш снимка шлюза и связка с приложением.
//!
//! Читаем только `Gateway::snapshot()`, пишем только `Gateway::apply_edit()`; сохранение на
//! диск идёт через очередь конфига по событию `GatewayPersist`. Рендер состояние не меняет.

pub mod edits;
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
    pub confirm_remove: Option<AccountId>,
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
    /// Хэш файла, от которого человек уже отказался: пока файл тот же, не спрашиваем снова.
    pub denied: Option<String>,
    pub known: Option<SystemTime>,
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
    /// Закреплённые чаты: ярлык аккаунта для подвала.
    pub pins: HashMap<ConversationId, String>,
    pub project: ProjectWatch,
}

impl GatewayView {
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
        let changed = watch.known.is_some() && watch.known != stamp;
        watch.known = stamp;
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
        self.state.gateway_view.project.known = self
            .project_config_path_of(&fresh)
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        self.config = fresh;
        if gateway_changed {
            self.restart_gateway();
        }
        self.rebuild_provider();
        self.refresh_gateway_view();
        self.open_trust_dialog_if_needed();
    }

    fn project_config_path_of(&self, config: &Config) -> Option<std::path::PathBuf> {
        use crate::config::Layer;
        config
            .source
            .layers
            .iter()
            .find(|l| l.layer == Layer::Project)
            .map(|l| l.path.clone())
    }

    /// Проектный слой ждёт подтверждения — показать диалог (если ничего другого на экране нет).
    pub(crate) fn open_trust_dialog_if_needed(&mut self) {
        if self.state.modal.is_some() {
            return;
        }
        if let Some(request) = self.config.source.pending_trust.clone()
            && self.state.gateway_view.project.denied.as_deref() != Some(request.hash.as_str())
        {
            self.state.modal = Some(Modal::Trust(Box::new(forms::TrustDialog::new(request))));
        }
    }

    /// Решение по проектному слою. Отказ ничего не пишет: слой остаётся заблокированным.
    pub(crate) fn resolve_trust(&mut self, dialog: forms::TrustDialog) {
        if !dialog.confirmed() {
            self.state.gateway_view.project.denied = Some(dialog.request.hash);
            self.state.push_ui_system(
                "Project config stays blocked: its accounts, routes and roles are not used.",
            );
            return;
        }
        let request = dialog.request;
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
    pub(crate) fn enable_gateway(&mut self) {
        let mut next = self.config.gateway_config();
        if next.accounts.is_empty() {
            next.accounts = GatewayConfig::from_legacy(&self.config).0.accounts;
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
            self.state.gateway_view.screen.complain(format!(
                "Cannot turn the gateway on: {}: {}",
                problem.path, problem.message
            ));
            return;
        }
        self.apply_gateway_config(next);
        if let Err(error) = crate::config::save_or_message(&self.config) {
            self.state.gateway_view.screen.complain(error);
            return;
        }
        self.restart_gateway();
        self.rebuild_provider();
        self.state
            .gateway_view
            .screen
            .say("Gateway is on. Add accounts with a, order them with K/J.");
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
                self.adopt_provider_after_edit();
            }
            GatewayEvent::FallbackTaken {
                from,
                to,
                class,
                source,
            } => {
                self.refresh_gateway_view();
                if matches!(source, CallSource::Chat | CallSource::SubAgent) {
                    let text = self.seam_for(from, to, class.code());
                    let mut seam = crate::provider::ChatMessage::ui_system(&text);
                    seam.status = Some(SEAM_STATUS.to_string());
                    self.state.focused_mut().messages.push(seam);
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

    /// Мастер дошёл до конца: секрет в хранилище (вне цикла событий), затем правка шлюза.
    pub(crate) fn submit_account_wizard(&mut self, mut wizard: Box<wizard::AccountWizard>) {
        let snapshot = self.state.gateway_view.snapshot.clone();
        let ids: Vec<AccountId> = snapshot
            .iter()
            .flat_map(|s| s.accounts.iter().map(|a| a.id.clone()))
            .chain(self.config.accounts.iter().map(|a| a.id.clone()))
            .collect();
        let labels: Vec<String> = snapshot
            .iter()
            .flat_map(|s| s.accounts.iter().map(|a| a.label.clone()))
            .collect();
        let root = wizard::login_home_root();
        let existing = wizard::Existing {
            ids: &ids,
            labels: &labels,
            home_root: &root,
        };
        let built = match wizard.build(&existing) {
            Ok(built) => built,
            Err(error) => {
                wizard.error = Some(error);
                self.state.modal = Some(Modal::AccountWizard(wizard));
                return;
            }
        };
        let secret = built
            .secret_id
            .clone()
            .map(|id| (id, wizard.secret.take_secret()));
        let tx = self.event_tx.clone();
        let plan = AccountPlan {
            account: built.account,
            login_command: built.login_command,
            warnings: built.warnings,
            stored: Ok(()),
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

    /// `/pin <аккаунт[/модель]>` и `/pin off`: чат закреплён, глобальный маршрут не меняется.
    pub(crate) fn pin_chat(&mut self, arg: &str) {
        let arg = arg.trim();
        let conversation = self.state.conversations.focused_id();
        if arg.is_empty() {
            let text = match self.state.gateway_view.pins.get(&conversation) {
                Some(label) => format!("This chat is pinned to {label}. /pin off releases it."),
                None => "Usage: /pin <account>[/model] | /pin off".to_string(),
            };
            self.state.push_ui_system(&text);
            return;
        }
        if arg.eq_ignore_ascii_case("off") {
            if self.state.gateway_view.pins.remove(&conversation).is_none() {
                self.state.push_ui_system("This chat is not pinned.");
                return;
            }
            self.rebuild_provider();
            self.state
                .push_ui_system("Unpinned: the chat follows the route again.");
            return;
        }
        if !self.gateway.is_enabled() {
            self.state
                .push_ui_system("Turn the gateway on first (/accounts), then pin.");
            return;
        }
        let snapshot = self.gateway.snapshot();
        let (name, model) = arg.split_once('/').unwrap_or((arg, ""));
        let Some(id) = find_account(&snapshot, name).cloned() else {
            self.state
                .push_ui_system(&format!("No account named '{name}'. See /accounts."));
            return;
        };
        let address = if model.is_empty() {
            id.to_string()
        } else {
            format!("{id}/{model}")
        };
        match self
            .gateway
            .provider_for_model_id(&address, CallSource::Chat)
        {
            Ok(provider) => {
                let label = snapshot
                    .accounts
                    .iter()
                    .find(|a| a.id == id)
                    .map_or_else(|| id.to_string(), |a| a.label.clone());
                switch_provider(
                    &mut self.state,
                    Some(provider),
                    &self.provider_window_epoch,
                    &self.event_tx,
                );
                self.state
                    .gateway_view
                    .pins
                    .insert(conversation, label.clone());
                self.state.push_ui_system(&format!(
                    "Pinned this chat to {label}. The route is unchanged; /pin off releases it."
                ));
            }
            Err(error) => self.state.push_ui_system(&error.to_string()),
        }
    }

    /// `/usage`: расход за сутки по аккаунтам.
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

    /// `/providers tools <имя> <native|prompt>` для аккаунта.
    pub(crate) fn set_account_tools(&mut self, name: &str, tools: crate::config::ToolProtocol) {
        let Some(snapshot) = self
            .state
            .gateway_view
            .snapshot
            .clone()
            .or_else(|| self.gateway.is_enabled().then(|| self.gateway.snapshot()))
        else {
            self.state
                .push_ui_system("Turn the gateway on first (/accounts).");
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
        return "Usage, last 24 h: no calls yet.".to_string();
    }
    let width = sums
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(7)
        .max(7);
    let mut out = format!(
        "Usage, last 24 h\n{:<width$}  {:>6} {:>6} {:>9} {:>8} {:>8} {:>6}",
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
        let event = tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv())
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
        let event = |source| GatewayEvent::FallbackTaken {
            from: AccountId::parse("a").unwrap(),
            to: AccountId::parse("b").unwrap(),
            class: crate::provider::failure::FailureClass::RateLimit { retry_after: None },
            source,
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
        app.config.source.pending_trust = Some(crate::config::TrustRequest {
            config_path: "x/.pooprusteek/config.toml".into(),
            key: "k".into(),
            hash: "h".into(),
            held_back: vec!["account evil (https://evil.test)".into()],
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
        let request = |hash: &str| crate::config::TrustRequest {
            config_path: "p".into(),
            key: "k".into(),
            hash: hash.into(),
            held_back: Vec::new(),
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
