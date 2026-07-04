//! Keys for the `/providers` surfaces: the full-screen management panel
//! (navigate, activate, remove, add) and the add-wizard modal. Provider
//! switching is instant — a config save plus `App::rebuild_provider`; no
//! network happens until the next turn.

use super::apply_text_key;
use crate::app::events::{Modal, View};
use crate::app::providers::{provider_rows, ProviderAddState, ProviderWizardStep};
use crate::app::App;
use crate::error::AppResult;

impl App {
    pub(super) async fn handle_providers_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        let rows = provider_rows(&self.config);
        let max = rows.len().saturating_sub(1);
        self.state.providers_view.selected = self.state.providers_view.selected.min(max);

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.providers_view.selected =
                    self.state.providers_view.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.providers_view.selected =
                    (self.state.providers_view.selected + 1).min(max);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(row) = rows.get(self.state.providers_view.selected) else {
                    return Ok(false);
                };
                if row.active {
                    self.state.providers_view.status_message =
                        format!("{} is already active", row.name);
                    return Ok(false);
                }
                self.config.active_provider =
                    (!row.builtin).then(|| row.name.clone());
                match crate::config::save(&self.config) {
                    Ok(()) => {
                        self.rebuild_provider();
                        self.state.providers_view.status_message =
                            if self.state.focused().provider.is_some() {
                                format!("{} is now active", row.name)
                            } else {
                                // Selected but unusable (deepseek without a
                                // token / endpoint init failure) — say so
                                // instead of a silent dead chat.
                                format!("{} selected, but no usable provider (check token/URL)", row.name)
                            };
                    }
                    Err(error) => {
                        self.state.providers_view.status_message =
                            format!("Failed to save config: {error}");
                    }
                }
            }
            KeyCode::Char('d') => {
                let Some(row) = rows.get(self.state.providers_view.selected) else {
                    return Ok(false);
                };
                if row.builtin {
                    self.state.providers_view.status_message =
                        "The built-in DeepSeek provider can't be removed".to_string();
                    return Ok(false);
                }
                let name = row.name.clone();
                let was_active = row.active;
                self.config.providers.retain(|entry| entry.name != name);
                if was_active {
                    self.config.active_provider = None;
                }
                match crate::config::save(&self.config) {
                    Ok(()) => {
                        if was_active {
                            self.rebuild_provider();
                        }
                        self.state.providers_view.status_message = format!(
                            "{name} removed{}",
                            if was_active { " — back on deepseek" } else { "" },
                        );
                    }
                    Err(error) => {
                        self.state.providers_view.status_message =
                            format!("Failed to save config: {error}");
                    }
                }
                let max = provider_rows(&self.config).len().saturating_sub(1);
                self.state.providers_view.selected = self.state.providers_view.selected.min(max);
            }
            KeyCode::Char('a') => {
                self.state.modal = Some(Modal::ProviderAdd(Box::new(ProviderAddState::new())));
            }
            _ => {}
        }
        Ok(false)
    }

    /// Key handling for the `/providers add` wizard. Text steps share
    /// `apply_text_key` (single-line — plain Enter always advances); Esc
    /// steps back, and from the first step closes the modal.
    pub(super) async fn handle_provider_add_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        mut wizard: Box<ProviderAddState>,
    ) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        let handled_as_text = match wizard.step {
            ProviderWizardStep::Name => apply_text_key(&mut wizard.name, key, false),
            ProviderWizardStep::BaseUrl => apply_text_key(&mut wizard.base_url, key, false),
            ProviderWizardStep::ApiKey => apply_text_key(&mut wizard.api_key, key, false),
            ProviderWizardStep::Model => apply_text_key(&mut wizard.model, key, false),
            ProviderWizardStep::Confirm => false,
        };

        if !handled_as_text {
            match key.code {
                KeyCode::Esc => {
                    match wizard.step {
                        ProviderWizardStep::Name => {
                            self.state.modal = None;
                            return Ok(false);
                        }
                        ProviderWizardStep::BaseUrl => wizard.step = ProviderWizardStep::Name,
                        ProviderWizardStep::ApiKey => wizard.step = ProviderWizardStep::BaseUrl,
                        ProviderWizardStep::Model => wizard.step = ProviderWizardStep::ApiKey,
                        ProviderWizardStep::Confirm => wizard.step = ProviderWizardStep::Model,
                    }
                    wizard.error = None;
                }
                KeyCode::Enter => match wizard.step {
                    ProviderWizardStep::Name => {
                        match crate::app::providers::validate_name(
                            wizard.name.buffer.trim(),
                            &self.config,
                        ) {
                            Ok(()) => {
                                wizard.error = None;
                                wizard.step = ProviderWizardStep::BaseUrl;
                            }
                            Err(error) => wizard.error = Some(error),
                        }
                    }
                    ProviderWizardStep::BaseUrl => {
                        match crate::app::providers::validate_base_url(
                            wizard.base_url.buffer.trim(),
                        ) {
                            Ok(_) => {
                                wizard.error = None;
                                wizard.step = ProviderWizardStep::ApiKey;
                            }
                            Err(error) => wizard.error = Some(error),
                        }
                    }
                    ProviderWizardStep::ApiKey => {
                        // Optional — blank means no key.
                        wizard.error = None;
                        wizard.step = ProviderWizardStep::Model;
                    }
                    ProviderWizardStep::Model => {
                        if wizard.model.buffer.trim().is_empty() {
                            wizard.error = Some("model can't be empty".to_string());
                        } else {
                            wizard.error = None;
                            wizard.step = ProviderWizardStep::Confirm;
                        }
                    }
                    ProviderWizardStep::Confirm => match wizard.build_entry(&self.config) {
                        Ok(entry) => {
                            let name = entry.name.clone();
                            self.config.providers.push(entry);
                            match crate::config::save(&self.config) {
                                Ok(()) => {
                                    self.state.modal = None;
                                    // Land on the panel with the new entry
                                    // selected, one Enter away from active.
                                    self.state.view = View::Providers;
                                    self.state.providers_view.selected =
                                        provider_rows(&self.config).len().saturating_sub(1);
                                    self.state.providers_view.status_message =
                                        format!("{name} added — Enter to activate");
                                    return Ok(false);
                                }
                                Err(error) => {
                                    self.config.providers.pop();
                                    wizard.error = Some(format!("Failed to save config: {error}"));
                                }
                            }
                        }
                        Err(error) => wizard.error = Some(error),
                    },
                },
                _ => {}
            }
        }
        self.state.modal = Some(Modal::ProviderAdd(wizard));
        Ok(false)
    }
}
