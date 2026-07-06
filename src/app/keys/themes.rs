//! Keys for the `/themes` surfaces: the gallery (navigate with live
//! preview, apply, create/edit/delete) and the step-by-step create wizard.
//! Applying is instant — `render` re-resolves the theme from config every
//! frame, so a config save is the whole effect.

use super::apply_text_key;
use crate::app::App;
use crate::app::events::View;
use crate::app::themes::{ThemeRowKind, ThemeWizardState, ThemeWizardStep, theme_rows};
use crate::error::AppResult;
use crate::tui::theme::{PRESETS, ROLES};

impl App {
    pub(super) async fn handle_themes_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        if self.state.themes.wizard.is_some() {
            self.handle_theme_wizard_key(key);
            return Ok(false);
        }

        let rows = theme_rows(&self.config);
        let max = rows.len().saturating_sub(1);
        self.state.themes.selected = self.state.themes.selected.min(max);

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.themes.selected = self.state.themes.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.themes.selected = (self.state.themes.selected + 1).min(max);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(row) = rows.get(self.state.themes.selected) else {
                    return Ok(false);
                };
                match row.kind {
                    ThemeRowKind::Create => {
                        self.state.themes.wizard = Some(ThemeWizardState::new());
                    }
                    _ if row.active => {
                        self.state.themes.status_message =
                            format!("{} is already active", row.label);
                    }
                    _ => self.apply_theme(&row.name.clone()),
                }
            }
            KeyCode::Char('n') | KeyCode::Char('a') => {
                self.state.themes.wizard = Some(ThemeWizardState::new());
            }
            KeyCode::Char('e') => {
                let Some(row) = rows.get(self.state.themes.selected) else {
                    return Ok(false);
                };
                match row.kind {
                    ThemeRowKind::Custom => {
                        if let Some(custom) = self
                            .config
                            .ui
                            .custom_themes
                            .iter()
                            .find(|theme| theme.name == row.name)
                        {
                            self.state.themes.wizard = Some(ThemeWizardState::edit(custom));
                        }
                    }
                    ThemeRowKind::Preset => {
                        self.state.themes.status_message =
                            "Built-in themes can't be edited — press n to create one based on it"
                                .to_string();
                    }
                    ThemeRowKind::Create => {}
                }
            }
            KeyCode::Char('d') => {
                let Some(row) = rows.get(self.state.themes.selected) else {
                    return Ok(false);
                };
                if row.kind != ThemeRowKind::Custom {
                    self.state.themes.status_message =
                        "Only custom themes can be deleted".to_string();
                    return Ok(false);
                }
                let name = row.name.clone();
                let was_active = row.active;
                self.config
                    .ui
                    .custom_themes
                    .retain(|theme| theme.name != name);
                if was_active {
                    self.config.ui.theme = "default".to_string();
                }
                match crate::config::save(&self.config) {
                    Ok(()) => {
                        self.state.themes.status_message = format!(
                            "{name} deleted{}",
                            if was_active {
                                " — back on Midnight"
                            } else {
                                ""
                            },
                        );
                    }
                    Err(error) => {
                        self.state.themes.status_message =
                            format!("Failed to save config: {error}");
                    }
                }
                let max = theme_rows(&self.config).len().saturating_sub(1);
                self.state.themes.selected = self.state.themes.selected.min(max);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Persist `name` as the active theme; the next frame renders with it.
    pub(crate) fn apply_theme(&mut self, name: &str) {
        self.config.ui.theme = name.to_string();
        self.state.themes.status_message = match crate::config::save(&self.config) {
            Ok(()) => format!("{name} is now active"),
            Err(error) => format!("Failed to save config: {error}"),
        };
    }

    /// Key handling for the theme wizard. Text steps share `apply_text_key`
    /// (single-line — plain Enter always advances); Esc steps back, and from
    /// the first step closes the wizard back to the gallery. Ctrl+S on any
    /// color step jumps straight to Confirm, keeping the remaining colors
    /// from the base palette.
    fn handle_theme_wizard_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        let Some(mut wizard) = self.state.themes.wizard.take() else {
            return;
        };

        let handled_as_text = match wizard.step {
            ThemeWizardStep::Name => apply_text_key(&mut wizard.name, key, false),
            ThemeWizardStep::Color(_) => apply_text_key(&mut wizard.hex_input, key, false),
            ThemeWizardStep::Base | ThemeWizardStep::Confirm => false,
        };

        if !handled_as_text {
            match key.code {
                KeyCode::Esc => match wizard.step {
                    ThemeWizardStep::Name => {
                        self.state.themes.wizard = None;
                        return;
                    }
                    ThemeWizardStep::Base => wizard.step = ThemeWizardStep::Name,
                    ThemeWizardStep::Color(0) => {
                        wizard.step = ThemeWizardStep::Base;
                        wizard.error = None;
                    }
                    ThemeWizardStep::Color(index) => wizard.enter_color_step(index - 1),
                    ThemeWizardStep::Confirm => wizard.enter_color_step(ROLES.len() - 1),
                },
                KeyCode::Up | KeyCode::Char('k') if wizard.step == ThemeWizardStep::Base => {
                    wizard.base_selected = wizard.base_selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if wizard.step == ThemeWizardStep::Base => {
                    wizard.base_selected = (wizard.base_selected + 1).min(PRESETS.len() - 1);
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let ThemeWizardStep::Color(index) = wizard.step {
                        // Commit the current value when it parses, then skip
                        // the remaining color steps.
                        if let Ok(normalized) =
                            crate::app::themes::validate_hex(&wizard.hex_input.buffer)
                        {
                            wizard.colors[index] = normalized;
                        }
                        wizard.error = None;
                        wizard.step = ThemeWizardStep::Confirm;
                    }
                }
                KeyCode::Enter => match wizard.step {
                    ThemeWizardStep::Name => {
                        match crate::app::themes::validate_theme_name(
                            wizard.name.buffer.trim(),
                            &self.config,
                            wizard.editing.as_deref(),
                        ) {
                            Ok(()) => {
                                wizard.error = None;
                                wizard.step = ThemeWizardStep::Base;
                            }
                            Err(error) => wizard.error = Some(error),
                        }
                    }
                    ThemeWizardStep::Base => wizard.enter_color_step(0),
                    ThemeWizardStep::Color(index) => {
                        match crate::app::themes::validate_hex(&wizard.hex_input.buffer) {
                            Ok(normalized) => {
                                wizard.colors[index] = normalized;
                                if index + 1 < ROLES.len() {
                                    wizard.enter_color_step(index + 1);
                                } else {
                                    wizard.error = None;
                                    wizard.step = ThemeWizardStep::Confirm;
                                }
                            }
                            Err(error) => wizard.error = Some(error),
                        }
                    }
                    ThemeWizardStep::Confirm => match self.finish_theme_wizard(&wizard) {
                        // The wizard slot stays `None` — finish landed on
                        // the gallery with the new theme applied.
                        Ok(()) => return,
                        Err(error) => wizard.error = Some(error),
                    },
                },
                _ => {}
            }
        }
        self.state.themes.wizard = Some(wizard);
    }

    /// Save the wizard's theme (insert, or replace the original when
    /// editing), make it active, and land on its gallery row. On failure the
    /// config is rolled back and the error goes back into the still-open
    /// wizard.
    fn finish_theme_wizard(&mut self, wizard: &ThemeWizardState) -> Result<(), String> {
        let custom = wizard.build_custom(&self.config)?;
        let name = custom.name.clone();
        let previous_active = self.config.ui.theme.clone();
        let themes = &mut self.config.ui.custom_themes;
        let replaced_index = wizard
            .editing
            .as_deref()
            .and_then(|original| themes.iter().position(|theme| theme.name == original));
        let previous_entry = match replaced_index {
            Some(index) => Some(std::mem::replace(&mut themes[index], custom)),
            None => {
                themes.push(custom);
                None
            }
        };
        self.config.ui.theme = name.clone();

        if let Err(error) = crate::config::save(&self.config) {
            // Roll back so a failed write doesn't leave a phantom theme
            // active in memory.
            self.config.ui.theme = previous_active;
            match (replaced_index, previous_entry) {
                (Some(index), Some(original)) => self.config.ui.custom_themes[index] = original,
                _ => self
                    .config
                    .ui
                    .custom_themes
                    .retain(|theme| theme.name != name),
            }
            return Err(format!("Failed to save config: {error}"));
        }

        self.state.themes.wizard = None;
        self.state.themes.selected = theme_rows(&self.config)
            .iter()
            .position(|row| row.name == name)
            .unwrap_or(0);
        self.state.themes.status_message = format!("{name} saved and applied");
        Ok(())
    }
}
