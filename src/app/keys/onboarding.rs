//! Keys for the full-screen onboarding view (`View::Onboarding`): token
//! entry, model toggle, and the Enter transition that hot-creates the
//! provider and lands on the chat view.

use crate::app::events::{
    AppEvent, DEEPSEEK_CHAT_URL, OnboardingAction, TOKEN_CONSOLE_SNIPPET, View,
};
use crate::app::{App, conversation};
use crate::error::AppResult;

impl App {
    pub(super) async fn handle_onboarding_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

        // Зажатая клавиша не должна открыть десяток вкладок.
        let fresh = key.kind != KeyEventKind::Repeat;

        match key.code {
            // Ctrl+C quits (handled by the main select! before we get here — belt-and-suspenders).
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(true);
            }
            KeyCode::Char('o') if fresh && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.run_onboarding_action(OnboardingAction::OpenSite);
            }
            KeyCode::Char('y') if fresh && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.run_onboarding_action(OnboardingAction::CopySnippet);
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
                    // Токен мог прийти через буфер, а у Windows есть его история.
                    self.state.status_message =
                        "Ready · your token may still be in the clipboard — copy something over it"
                            .to_string();
                    self.state.view = View::Chat;
                }
                // If submit() returned None it set an error; the view stays on onboarding.
            }
            _ => {}
        }
        Ok(false)
    }

    /// Браузер и буфер обмена — внешние процессы, на цикле событий их не ждём.
    fn run_onboarding_action(&self, action: OnboardingAction) {
        let event_tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = match action {
                OnboardingAction::OpenSite => open::that_detached(DEEPSEEK_CHAT_URL),
                OnboardingAction::CopySnippet => crate::clipboard::copy(TOKEN_CONSOLE_SNIPPET),
            };
            if let Err(error) = &result {
                tracing::warn!("onboarding {action:?} failed: {error}");
            }
            let _ = event_tx.send(AppEvent::OnboardingActionDone {
                action,
                ok: result.is_ok(),
            });
        });
    }
}
