//! Bracketed-paste handling: route a whole pasted chunk into whatever text
//! field currently has focus.
//!
//! `tui::init` enables bracketed paste, so the terminal delivers a paste as a
//! single `Event::Paste(String)` instead of a stream of `Char`/`Enter` key
//! events. That fixes the old failure mode — a multi-line paste whose first
//! newline fired an early submit — but it also means every text surface that
//! used to receive those synthetic key events must now be routed here, or it
//! would silently lose paste entirely. So this mirrors `handle_key`'s dispatch
//! precedence (onboarding → modal → view) and inserts into the same field the
//! keystroke would have edited.
//!
//! Single-line fields (names, URLs, a token, a fuzzy filter) drop embedded
//! newlines; the multi-line surfaces (the chat prompt, the `/mcp add` JSON
//! box, the wizard "extra" step) keep them.

use crate::app::App;
use crate::app::events::{Modal, View};
use crate::app::mcp_add::{McpAddState, WizardStep};
use crate::app::providers::ProviderWizardStep;
use crate::app::search::SearchFocus;
use crate::app::themes::ThemeWizardStep;

/// Strip carriage returns always (so Windows `\r\n` never leaves a stray CR);
/// additionally drop newlines for single-line fields.
fn sanitize(text: &str, allow_newline: bool) -> String {
    text.chars()
        .filter(|&c| c != '\r' && (allow_newline || c != '\n'))
        .collect()
}

impl App {
    pub(crate) fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }

        // Onboarding view captures everything — single-line token entry.
        if self.state.view == View::Onboarding {
            for c in sanitize(&text, false).chars() {
                self.state.onboarding.insert(c);
            }
            return;
        }

        // A modal owns the screen when present.
        if let Some(modal) = self.state.modal.as_mut() {
            match modal {
                Modal::Picker(picker) => {
                    let mut s = picker.search.clone();
                    s.push_str(&sanitize(&text, false));
                    picker.update_search(s);
                }
                Modal::Question(qs) if qs.is_custom_mode => {
                    let clean = sanitize(&text, false);
                    let byte_pos =
                        crate::util::char_to_byte_pos(&qs.custom_input, qs.custom_cursor);
                    qs.custom_input.insert_str(byte_pos, &clean);
                    qs.custom_cursor += clean.chars().count();
                }
                Modal::McpAdd(state) => match state {
                    McpAddState::PasteJson { input, .. } => {
                        input.insert_str(&sanitize(&text, true));
                    }
                    McpAddState::Wizard(wiz) => match wiz.step {
                        WizardStep::Name => wiz.name.insert_str(&sanitize(&text, false)),
                        WizardStep::Primary => wiz.primary.insert_str(&sanitize(&text, false)),
                        // Env vars / headers — one entry per line, so keep them.
                        WizardStep::Extra => wiz.extra.insert_str(&sanitize(&text, true)),
                        WizardStep::Transport | WizardStep::Confirm => {}
                    },
                    McpAddState::ChooseMethod { .. } => {}
                },
                Modal::ProviderAdd(wiz) => {
                    let clean = sanitize(&text, false);
                    match wiz.step {
                        ProviderWizardStep::Name => wiz.name.insert_str(&clean),
                        ProviderWizardStep::BaseUrl => wiz.base_url.insert_str(&clean),
                        ProviderWizardStep::ApiKey => wiz.api_key.insert_str(&clean),
                        ProviderWizardStep::Model => wiz.model.insert_str(&clean),
                        ProviderWizardStep::Protocol | ProviderWizardStep::Confirm => {}
                    }
                }
                // No text field: tool-approval, delete-sessions, confirm, and a
                // non-custom (option-list) question.
                _ => {}
            }
            return;
        }

        // View-specific text inputs (no modal in front).
        match self.state.view {
            View::Search => {
                if self.state.search.focus == SearchFocus::Query {
                    self.state.search.query.insert_str(&sanitize(&text, false));
                }
            }
            View::Themes => {
                if let Some(wiz) = self.state.themes.wizard.as_mut() {
                    let clean = sanitize(&text, false);
                    match wiz.step {
                        ThemeWizardStep::Name => wiz.name.insert_str(&clean),
                        ThemeWizardStep::Color(_) => wiz.hex_input.insert_str(&clean),
                        ThemeWizardStep::Base | ThemeWizardStep::Confirm => {}
                    }
                }
            }
            // Providers/Mcp management screens open their text entry as a modal
            // (handled above); their base views are list navigation only.
            View::Providers | View::Mcp => {}
            _ => {
                // Chat prompt (the default view). A multi-line paste collapses
                // to a `[Pasted #N, L lines]` chip so a big block neither floods
                // the prompt nor (with the newlines now off in stored content)
                // fires an early submit. Single-line pastes insert inline.
                let clean = sanitize(&text, true);
                if clean.contains('\n') {
                    self.state.input.insert_paste(clean);
                } else {
                    self.state.input.insert_str(&clean);
                }
                self.refresh_autocomplete();
            }
        }
    }
}
