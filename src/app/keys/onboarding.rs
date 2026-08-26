//! Keys for the full-screen onboarding view (`View::Onboarding`): token
//! entry, model toggle, and the Enter transition that hot-creates the
//! provider and lands on the chat view.

use crate::app::events::View;
use crate::app::{App, conversation};
use crate::error::AppResult;

impl App {
    pub(super) async fn handle_onboarding_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            // Ctrl+C quits (handled by the main select! before we get here — belt-and-suspenders).
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(true);
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Left | KeyCode::BackTab => {
                self.state.onboarding.toggle_model();
            }
            KeyCode::Backspace => {
                self.state.onboarding.backspace();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.state.onboarding.insert(c);
            }
            KeyCode::Enter => {
                if let Some(token) = self.state.onboarding.submit() {
                    // Commit token + model to config and save.
                    self.config.provider.token = token;
                    self.config.provider.model = self.state.onboarding.model_str().to_string();
                    if let Err(e) = crate::config::save(&self.config) {
                        tracing::warn!("Onboarding: failed to save config: {e}");
                    }

                    // Hot-create the provider so the app works without restart.
                    let Some(provider) = crate::provider::build_provider(&self.config) else {
                        // build_provider logged the cause via tracing.
                        self.state.onboarding.error =
                            Some("Failed to initialize provider — see log");
                        return Ok(false);
                    };

                    // Swap in a fresh main conversation carrying the provider —
                    // safe, the current one has no messages yet.
                    self.state.conversations = conversation::Conversations::new(
                        conversation::Conversation::fresh_main(Some(provider)),
                    );
                    self.state.status_message = "Ready".to_string();
                    self.state.view = View::Chat;
                }
                // If submit() returned None it set an error; the view stays on onboarding.
            }
            _ => {}
        }
        Ok(false)
    }
}
