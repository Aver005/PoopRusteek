//! Key handling, split by input surface. This module's `handle_key` is the
//! single entry point (called from the event loop); it only decides *which
//! surface* owns the keystroke and delegates:
//!
//! - [`onboarding`] — the full-screen first-run view captures everything
//! - [`autocomplete`] — Tab/arrows/Enter while the dropdown is open
//! - [`modal`] — whichever modal is on screen (approval, picker, question,
//!   delete-sessions, confirm, `/mcp add`)
//! - [`mcp`] — the `View::Mcp` management screen (+ its auth picker)
//! - [`chat`] — the default chat view: editing, history, scroll, submit;
//!   command effects live in [`dispatch`] (`CommandResult` is the intent,
//!   `apply_command_result` is the interpreter)
//!
//! Pure key→intent decoding lives next to each surface (see
//! `modal::approval_key` / `modal::confirm_key`, and the existing
//! `events::handle_picker_key` family) so it stays testable without an
//! `App`.

mod autocomplete;
mod chat;
mod dispatch;
mod mcp;
mod modal;
mod onboarding;
mod providers;
mod search;

use crate::app::events::View;
use crate::app::input::InputState;
use crate::app::App;
use crate::error::AppResult;

/// Apply a standard text-editing keystroke to `input`; returns whether the
/// key was consumed. Shared by every `/mcp add` text-entry step (JSON
/// paste, name, command/URL, env/headers) so each step's own handler only
/// needs to add its Enter/Esc transition on top. `allow_newline` gates
/// Shift+Enter — off for single-line fields (name, command/URL) so plain
/// Enter always means "submit" there, mirroring the main chat input's
/// Shift+Enter-for-newline convention (`chat::handle_chat_key`,
/// `KeyCode::Enter if SHIFT`).
fn apply_text_key(input: &mut InputState, key: crossterm::event::KeyEvent, allow_newline: bool) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::Enter if allow_newline && key.modifiers.contains(KeyModifiers::SHIFT) => {
            input.insert_newline();
            true
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.insert_char(c);
            true
        }
        KeyCode::Backspace => { input.backspace(); true }
        KeyCode::Delete => { input.delete_forward(); true }
        KeyCode::Left => { input.move_left(false, key.modifiers.contains(KeyModifiers::CONTROL)); true }
        KeyCode::Right => { input.move_right(false, key.modifiers.contains(KeyModifiers::CONTROL)); true }
        KeyCode::Home => { input.move_home(false); true }
        KeyCode::End => { input.move_end(false); true }
        _ => false,
    }
}

impl App {
    pub(crate) async fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        // Onboarding view captures all keys while it is active.
        if self.state.view == View::Onboarding {
            return self.handle_onboarding_key(key).await;
        }

        // Global Tab/Up/Down are intercepted by autocomplete when visible.
        if self.autocomplete_intercepts(key) {
            return Ok(false);
        }

        if self.state.modal.is_some() {
            return self.handle_modal_key(key).await;
        }

        if self.state.view == View::Mcp {
            return self.handle_mcp_key(key).await;
        }

        if self.state.view == View::Providers {
            return self.handle_providers_key(key).await;
        }

        if self.state.view == View::Search {
            return self.handle_search_key(key).await;
        }

        self.handle_chat_key(key).await
    }
}
