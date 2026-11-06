//! Клавиши экрана аккаунтов и его модалок: мастер, скрытый ввод секрета, ограничения,
//! подтверждение проектного слоя. Правки идут только через `Gateway::apply_edit`.

use super::apply_text_key;
use crate::app::App;
use crate::app::events::{Modal, View};
use crate::app::gateway_view::forms::{LimitsForm, SecretPrompt, TrustDialog};
use crate::app::gateway_view::wizard::{
    AccountWizard, Existing, HiddenInput, Step, login_home_root,
};
use crate::app::gateway_view::{Focus, edits, rows};
use crate::app::list::{ListNav, page_rows, rows as list_rows};
use crate::error::AppResult;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Клавиша в скрытом поле: печать, Backspace, Ctrl+U. `true` — клавиша съедена.
fn hidden_key(field: &mut HiddenInput, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            field.clear();
            true
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            field.push(c);
            true
        }
        KeyCode::Backspace => {
            field.pop();
            true
        }
        _ => false,
    }
}

impl App {
    pub(crate) async fn handle_accounts_key(&mut self, key: KeyEvent) -> AppResult<bool> {
        if let Some(id) = self.state.gateway_view.screen.confirm_remove.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.gateway_edit(
                    crate::gateway::handle::ConfigEdit::RemoveAccount(id.clone()),
                    &format!(
                        "Removed {id}. Its stored key, if any, stays in the keyring (/secrets rm)."
                    ),
                );
            } else {
                self.state.gateway_view.screen.say("Not removed.");
            }
            return Ok(false);
        }
        if !self.gateway.is_enabled() {
            return Ok(self.handle_gateway_off_key(key));
        }
        self.state.gateway_view.screen.notice = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.state.gateway_view.screen.detail || self.state.gateway_view.screen.history {
                    self.state.gateway_view.screen.detail = false;
                    self.state.gateway_view.screen.history = false;
                } else {
                    self.state.view = View::Chat;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let forward = key.code == KeyCode::Tab;
                let screen = &mut self.state.gateway_view.screen;
                screen.focus = screen.focus.next(forward);
            }
            KeyCode::Char('h') => {
                let screen = &mut self.state.gateway_view.screen;
                screen.history = !screen.history;
            }
            _ => match self.state.gateway_view.screen.focus {
                Focus::Accounts => self.accounts_focus_key(key),
                Focus::Routes => self.routes_focus_key(key),
                Focus::Roles => self.roles_focus_key(key),
            },
        }
        Ok(false)
    }

    /// Шлюз выключен: экран только объясняет и предлагает включить.
    fn handle_gateway_off_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.state.view = View::Chat,
            KeyCode::Enter | KeyCode::Char('g') => self.enable_gateway(),
            KeyCode::Char('a') => {
                self.enable_gateway();
                if self.gateway.is_enabled() {
                    self.state.modal = Some(Modal::AccountWizard(Box::default()));
                }
            }
            _ => {}
        }
        false
    }

    fn accounts_focus_key(&mut self, key: KeyEvent) {
        let Some(snapshot) = self.state.gateway_view.snapshot.clone() else {
            return;
        };
        let selected = self.state.gateway_view.screen.selected;
        let count = snapshot.accounts.len();
        if let Some(nav) = ListNav::from_code_vim(key.code) {
            let page = page_rows(self.state.terminal_rows, list_rows::PROVIDER);
            self.state.gateway_view.screen.selected = nav.move_within(selected, count, page);
            return;
        }
        let current = snapshot.accounts.get(selected);
        match key.code {
            KeyCode::Char('K') | KeyCode::Char('J') => {
                let delta = if key.code == KeyCode::Char('K') {
                    -1
                } else {
                    1
                };
                if let Some((edit, to)) = edits::move_account(&snapshot, selected, delta)
                    && self.gateway_edit(edit, "")
                {
                    self.state.gateway_view.screen.selected = to;
                    self.rank_main_route_by_list();
                }
            }
            KeyCode::Char(' ') => {
                if let Some(edit) = edits::toggle_account(&snapshot, selected) {
                    self.gateway_edit(edit, "");
                }
            }
            KeyCode::Enter => self.open_account_detail(),
            KeyCode::Char('a') => {
                self.state.modal = Some(Modal::AccountWizard(Box::default()));
            }
            KeyCode::Char('d') => {
                if let Some(view) = current {
                    self.state.gateway_view.screen.confirm_remove = Some(view.id.clone());
                }
            }
            KeyCode::Char('e') => {
                if let Some(account) = current.and_then(|v| self.account_config(&v.id)) {
                    self.state.modal = Some(Modal::AccountLimits(Box::new(
                        LimitsForm::for_account(&account),
                    )));
                }
            }
            KeyCode::Char('r') => {
                if let (Some(view), Some(route)) = (current, edits::main_route(&snapshot)) {
                    match edits::toggle_membership(route, &view.id) {
                        Some(edit) => {
                            let verb = if edits::in_route(route, &view.id) {
                                "Left"
                            } else {
                                "Joined"
                            };
                            self.gateway_edit(
                                edit,
                                &format!("{verb} route {}: {}.", route.id, view.label),
                            );
                        }
                        None => self
                            .state
                            .gateway_view
                            .screen
                            .complain("A route needs at least one step."),
                    }
                }
            }
            KeyCode::Char('p') => {
                if let Some(view) = current {
                    let id = view.id.to_string();
                    self.pin_chat(&id);
                    self.state
                        .gateway_view
                        .screen
                        .say(format!("Pinned this chat to {}.", view.label));
                }
            }
            KeyCode::Char('u') => {
                // Правка без изменений: движок заново собирает адаптер недоступного аккаунта.
                if let Some(view) = current {
                    let edit = crate::gateway::handle::ConfigEdit::SetEnabled {
                        id: view.id.clone(),
                        enabled: view.enabled,
                    };
                    self.gateway_edit(edit, &format!("Rebuilt {}.", view.label));
                }
            }
            _ => {}
        }
    }

    /// Порядок списка — приоритет: главный маршрут выстраивается за ним.
    fn rank_main_route_by_list(&mut self) {
        let Some(snapshot) = self.gateway.is_enabled().then(|| self.gateway.snapshot()) else {
            return;
        };
        if let Some(route) = edits::main_route(&snapshot)
            && let Some(edit) = edits::resort_route(&snapshot, route)
        {
            self.gateway_edit(edit, "");
        }
    }

    /// Enter: подробности; у аккаунта, что просит входа, — команда входа.
    fn open_account_detail(&mut self) {
        let screen = &mut self.state.gateway_view.screen;
        screen.detail = !screen.detail;
        let Some(view) = self
            .state
            .gateway_view
            .snapshot
            .as_ref()
            .and_then(|s| s.accounts.get(screen.selected))
        else {
            return;
        };
        let needs_login = matches!(
            view.status,
            crate::gateway::handle::AccountStatus::NeedsAttention { .. }
                | crate::gateway::handle::AccountStatus::Unavailable { .. }
        );
        let Some(tool) = view.provider.cli_tool().filter(|_| needs_login) else {
            return;
        };
        let home = self
            .config
            .accounts
            .iter()
            .find(|a| a.id == view.id)
            .and_then(|a| a.home.clone())
            .unwrap_or_else(|| login_home_root().join(view.id.as_str()));
        let command = crate::app::gateway_view::wizard::login_command_for(tool, &home);
        self.state
            .gateway_view
            .screen
            .say(format!("Log in again, then press u: {command}"));
    }

    fn routes_focus_key(&mut self, key: KeyEvent) {
        let Some(snapshot) = self.state.gateway_view.snapshot.clone() else {
            return;
        };
        let screen = &mut self.state.gateway_view.screen;
        let route = snapshot.routes.get(screen.route);
        match key.code {
            KeyCode::Left => screen.route = screen.route.saturating_sub(1),
            KeyCode::Right => {
                screen.route = (screen.route + 1).min(snapshot.routes.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => screen.step = screen.step.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                let last = route.map_or(0, |r| r.steps.len().saturating_sub(1));
                screen.step = (screen.step + 1).min(last);
            }
            KeyCode::Char('K') | KeyCode::Char('J') => {
                let delta = if key.code == KeyCode::Char('K') {
                    -1
                } else {
                    1
                };
                if let Some(route) = route
                    && let Some((edit, to)) = edits::move_step(route, screen.step, delta)
                    && self.gateway_edit(edit, "")
                {
                    self.state.gateway_view.screen.step = to;
                }
            }
            KeyCode::Char('x') => {
                if let Some(route) = route {
                    match edits::remove_step(route, screen.step) {
                        Some(edit) => {
                            self.gateway_edit(edit, "Removed the step.");
                        }
                        None => screen.complain("A route needs at least one step."),
                    }
                }
            }
            _ => {}
        }
    }

    fn roles_focus_key(&mut self, key: KeyEvent) {
        let Some(snapshot) = self.state.gateway_view.snapshot.clone() else {
            return;
        };
        let screen = &mut self.state.gateway_view.screen;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => screen.role = screen.role.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                screen.role = (screen.role + 1).min(rows::ROLES.len() - 1);
            }
            KeyCode::Left | KeyCode::Right => {
                let role = rows::ROLES[screen.role.min(rows::ROLES.len() - 1)];
                if let Some(edit) = edits::cycle_role(&snapshot, role, key.code == KeyCode::Right) {
                    self.gateway_edit(edit, "");
                }
            }
            _ => {}
        }
    }

    // ── Мастер ──────────────────────────────────────────────────────────

    pub(crate) async fn handle_account_wizard_key(
        &mut self,
        key: KeyEvent,
        mut wizard: Box<AccountWizard>,
    ) -> AppResult<bool> {
        let handled = match wizard.step {
            Step::Label => apply_text_key(&mut wizard.label, key, false),
            Step::BaseUrl => apply_text_key(&mut wizard.base_url, key, false),
            Step::Model => apply_text_key(&mut wizard.model, key, false),
            Step::Secret => hidden_key(&mut wizard.secret, key),
            _ => false,
        };
        if handled {
            self.state.modal = Some(Modal::AccountWizard(wizard));
            return Ok(false);
        }
        let choices = match wizard.step {
            Step::Kind => Some(crate::app::gateway_view::wizard::KINDS.len()),
            Step::Protocol => Some(crate::app::gateway_view::wizard::PROTOCOLS.len()),
            Step::Login | Step::Consent => Some(2),
            _ => None,
        };
        if let (Some(len), Some(nav)) = (choices, ListNav::from_code_vim(key.code)) {
            let page = page_rows(self.state.terminal_rows, list_rows::PROVIDER);
            let slot = match wizard.step {
                Step::Kind => &mut wizard.kind,
                Step::Protocol => &mut wizard.protocol,
                Step::Login => &mut wizard.login,
                _ => &mut wizard.consent_choice,
            };
            *slot = nav.move_within(*slot, len, page);
            self.state.modal = Some(Modal::AccountWizard(wizard));
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => {
                if !wizard.back() {
                    return Ok(false);
                }
            }
            KeyCode::Enter => {
                let (ids, labels) = self.existing_accounts();
                let root = login_home_root();
                let existing = Existing {
                    ids: &ids,
                    labels: &labels,
                    home_root: &root,
                };
                if let Ok(true) = wizard.advance(&existing) {
                    self.submit_account_wizard(wizard);
                    return Ok(false);
                }
            }
            _ => {}
        }
        self.state.modal = Some(Modal::AccountWizard(wizard));
        Ok(false)
    }

    fn existing_accounts(&self) -> (Vec<crate::gateway::types::AccountId>, Vec<String>) {
        match &self.state.gateway_view.snapshot {
            Some(snapshot) => (
                snapshot.accounts.iter().map(|a| a.id.clone()).collect(),
                snapshot.accounts.iter().map(|a| a.label.clone()).collect(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }

    // ── Секрет под маской ───────────────────────────────────────────────

    pub(crate) async fn handle_secret_prompt_key(
        &mut self,
        key: KeyEvent,
        mut prompt: Box<SecretPrompt>,
    ) -> AppResult<bool> {
        if hidden_key(&mut prompt.value, key) {
            self.state.modal = Some(Modal::SecretPrompt(prompt));
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => {
                self.state.status_message = "Secret not stored".to_string();
            }
            KeyCode::Enter if prompt.value.is_empty() => {
                prompt.error = Some("the value is empty".to_string());
                self.state.modal = Some(Modal::SecretPrompt(prompt));
            }
            KeyCode::Enter => {
                let name = prompt.name.clone();
                let value = prompt.value.take_secret();
                let tx = self.event_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let message =
                        crate::secrets::ops::set(&crate::secrets::process_store(), &name, value)
                            .unwrap_or_else(|error| format!("Error: {error}"));
                    let _ = tx.send(crate::app::events::AppEvent::UpdateStatus {
                        message,
                        notable: true,
                    });
                });
            }
            _ => self.state.modal = Some(Modal::SecretPrompt(prompt)),
        }
        Ok(false)
    }

    // ── Ограничения и избранные модели ──────────────────────────────────

    pub(crate) async fn handle_limits_key(
        &mut self,
        key: KeyEvent,
        mut form: Box<LimitsForm>,
    ) -> AppResult<bool> {
        if apply_text_key(form.current(), key, false) {
            self.state.modal = Some(Modal::AccountLimits(form));
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Up | KeyCode::BackTab => {
                form.step(false);
                self.state.modal = Some(Modal::AccountLimits(form));
            }
            KeyCode::Down | KeyCode::Tab => {
                form.step(true);
                self.state.modal = Some(Modal::AccountLimits(form));
            }
            KeyCode::Enter => {
                let saved = match self.account_config(&form.account) {
                    Some(account) => form.apply_to(&account).and_then(|next| {
                        self.gateway
                            .apply_edit(crate::gateway::handle::ConfigEdit::UpsertAccount(
                                Box::new(next),
                            ))
                            .map_err(|error| error.to_string())
                    }),
                    None => Err("the account is gone".to_string()),
                };
                match saved {
                    Ok(()) => {
                        self.refresh_gateway_view();
                        self.state
                            .gateway_view
                            .screen
                            .say(format!("Saved limits for {}.", form.name));
                    }
                    Err(error) => {
                        form.error = Some(error);
                        self.state.modal = Some(Modal::AccountLimits(form));
                    }
                }
            }
            _ => self.state.modal = Some(Modal::AccountLimits(form)),
        }
        Ok(false)
    }

    // ── Подтверждение проектного слоя ───────────────────────────────────

    pub(crate) async fn handle_trust_key(
        &mut self,
        key: KeyEvent,
        mut dialog: Box<TrustDialog>,
    ) -> AppResult<bool> {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                dialog.toggle();
                self.state.modal = Some(Modal::Trust(dialog));
            }
            // Подтвердить можно только выбрав «Trust» и нажав Enter: `y` и случайный Enter не годятся.
            KeyCode::Enter => self.resolve_trust(*dialog),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                dialog.choice = crate::app::gateway_view::forms::TrustChoice::Deny;
                self.resolve_trust(*dialog);
            }
            _ => self.state.modal = Some(Modal::Trust(dialog)),
        }
        Ok(false)
    }
}
