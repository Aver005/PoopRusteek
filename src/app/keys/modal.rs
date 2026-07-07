//! Key handling for whichever modal owns the keyboard. Decoding is pure
//! where the mapping is more than trivial — `approval_key`/`confirm_key`
//! here, plus the existing `events::handle_picker_key` /
//! `handle_question_key` / `handle_delete_sessions_key` family — so the
//! key→intent layer stays testable without an `App`. The `App` methods
//! below only apply effects.

use crate::app::events::{self, Modal};
use crate::app::{App, conversation};
use crate::error::AppResult;
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashSet;

/// What a keystroke means to the tool-approval modal.
#[derive(Debug, PartialEq, Eq)]
enum ApprovalKey {
    Approve,
    Deny,
    ToggleAlways,
    ScrollUp,
    ScrollDown,
}

fn approval_key(code: KeyCode) -> Option<ApprovalKey> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(ApprovalKey::Approve),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalKey::Deny),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalKey::ToggleAlways),
        KeyCode::Up => Some(ApprovalKey::ScrollUp),
        KeyCode::Down => Some(ApprovalKey::ScrollDown),
        _ => None,
    }
}

/// What a keystroke means to the generic confirm modal: `Some(true)` =
/// confirmed, `Some(false)` = cancelled, `None` = ignored.
fn confirm_key(code: KeyCode) -> Option<bool> {
    match code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Some(false),
        _ => None,
    }
}

impl App {
    /// Route a keystroke to the currently-open modal. Takes the modal out of
    /// state and moves it back when it stays open — no per-keystroke clone
    /// of the modal payload (tool-approval `arguments` can be large).
    pub(super) async fn handle_modal_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        let Some(modal) = self.state.modal.take() else {
            return Ok(false);
        };
        match modal {
            Modal::ToolApproval {
                tool_name,
                arguments,
                scroll_offset,
                always_allow,
            } => {
                match approval_key(key.code) {
                    Some(ApprovalKey::Approve) => {
                        if always_allow {
                            self.state.approved_tools.insert(tool_name.clone());
                            crate::whitelist::persist_approval(&tool_name);
                        }
                        if let Some(request) = self.state.pending_tool_approval.take() {
                            request.resolve(true).await;
                        }
                        self.state.focused_mut().generation.active = true;
                        self.state.status_message = format!("Running {}", tool_name);
                        self.present_next_interaction().await;
                    }
                    Some(ApprovalKey::Deny) => {
                        if let Some(request) = self.state.pending_tool_approval.take() {
                            request.resolve(false).await;
                        }
                        self.state.focused_mut().generation.active = true;
                        self.state.status_message = format!("Denied {}", tool_name);
                        self.present_next_interaction().await;
                    }
                    Some(ApprovalKey::ToggleAlways) => {
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset,
                            always_allow: !always_allow,
                        });
                    }
                    Some(ApprovalKey::ScrollUp) => {
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset: scroll_offset.saturating_sub(3),
                            always_allow,
                        });
                    }
                    Some(ApprovalKey::ScrollDown) => {
                        let arg_lines = arguments.lines().count();
                        let max_visible = 12usize;
                        let max_scroll = arg_lines.saturating_sub(max_visible);
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset: (scroll_offset + 3).min(max_scroll),
                            always_allow,
                        });
                    }
                    None => {
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset,
                            always_allow,
                        });
                    }
                }
                Ok(false)
            }
            Modal::Picker(mut picker) => {
                // Ctrl+A: toggle select all filtered items
                if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    let filtered_count = picker.items.len();
                    if filtered_count > 0 && picker.persistent_checked.len() >= filtered_count {
                        picker.persistent_checked.clear();
                    } else {
                        picker.persistent_checked =
                            picker.items.iter().map(|item| item.value.clone()).collect();
                    }
                    picker.sync_checked();
                    self.state.modal = Some(Modal::Picker(picker));
                    return Ok(false);
                }
                match key.code {
                    KeyCode::Char(c) if !matches!(c, 'j' | 'k' | ' ') => {
                        let mut s = picker.search.clone();
                        s.push(c);
                        picker.update_search(s);
                        self.state.modal = Some(Modal::Picker(picker));
                        return Ok(false);
                    }
                    KeyCode::Backspace => {
                        let mut s = picker.search.clone();
                        s.pop();
                        picker.update_search(s);
                        self.state.modal = Some(Modal::Picker(picker));
                        return Ok(false);
                    }
                    _ => {}
                }
                let kind = picker.kind.clone();
                let action = events::handle_picker_key(&mut picker, key.code);
                match action {
                    events::PickerAction::Selected(indices) => {
                        self.apply_picker_selection(kind, &picker, indices).await?;
                    }
                    events::PickerAction::Cancelled => {}
                    events::PickerAction::None => {
                        self.state.modal = Some(Modal::Picker(picker));
                    }
                }
                Ok(false)
            }
            Modal::DeleteSessions(mut st) => {
                match events::handle_delete_sessions_key(&mut st, key.code) {
                    events::DeleteAction::None => {
                        self.state.modal = Some(Modal::DeleteSessions(st));
                    }
                    events::DeleteAction::Close => {
                        self.present_next_interaction().await;
                    }
                    events::DeleteAction::Execute { ids, scope } => {
                        self.execute_delete_sessions(ids, scope, st.entries).await;
                        self.present_next_interaction().await;
                    }
                }
                Ok(false)
            }
            Modal::Question(mut qs) => {
                let result = events::handle_question_key(&mut qs, key.code);
                if let Some(answer) = result {
                    if let Some(request) = self.state.pending_question.take() {
                        request.resolve(answer).await;
                    }
                    self.state.focused_mut().generation.active = true;
                    self.state.status_message = "Answer received".to_string();
                    self.present_next_interaction().await;
                } else {
                    self.state.modal = Some(Modal::Question(qs));
                    self.state.status_message = "Answering question...".to_string();
                }
                Ok(false)
            }
            Modal::Confirm(cs) => {
                match confirm_key(key.code) {
                    Some(true) => match cs.action {
                        events::ConfirmAction::Logout => self.execute_logout().await,
                        events::ConfirmAction::Wipe => self.execute_wipe().await,
                        events::ConfirmAction::Update => self.start_manual_update(),
                    },
                    Some(false) => {}
                    None => {
                        self.state.modal = Some(Modal::Confirm(cs));
                    }
                }
                Ok(false)
            }
            Modal::McpAdd(state) => self.handle_mcp_add_key(key, state).await,
            Modal::ProviderAdd(state) => self.handle_provider_add_key(key, state).await,
        }
    }

    /// Apply a confirmed picker selection according to the picker's kind.
    async fn apply_picker_selection(
        &mut self,
        kind: events::PickerKind,
        picker: &events::PickerState,
        indices: Vec<usize>,
    ) -> AppResult<()> {
        match kind {
            events::PickerKind::Whitelist => {
                let selected_names: HashSet<String> = indices
                    .iter()
                    .filter_map(|&i| picker.items.get(i))
                    .map(|item| item.value.clone())
                    .collect();
                if let Err(e) = crate::whitelist::save(&selected_names) {
                    tracing::warn!("Failed to save whitelist: {e}");
                }
                self.state.approved_tools = selected_names;
            }
            events::PickerKind::Skills => {
                let Ok(mut config) = crate::config::load() else {
                    return Ok(());
                };
                let enabled: Vec<String> = indices
                    .iter()
                    .filter_map(|&i| picker.items.get(i))
                    .map(|item| item.value.clone())
                    .collect();
                config.skills.enabled = enabled.clone();
                if let Err(e) = crate::config::save(&config) {
                    tracing::warn!("Failed to save skills config: {e}");
                }
                self.config.skills.enabled = enabled.clone();
                for skill in &mut self.skills {
                    skill.enabled = enabled.contains(&skill.slug) || enabled.contains(&skill.name);
                }
                self.tools.update_skills(self.skills.clone());
            }
            events::PickerKind::Chats => {
                let target = indices
                    .first()
                    .and_then(|&i| picker.items.get(i))
                    .and_then(|item| item.value.parse::<u64>().ok())
                    .map(conversation::ConversationId);
                if let Some(target) = target {
                    self.switch_to(target);
                }
            }
            events::PickerKind::Agents => {
                let target = indices
                    .first()
                    .and_then(|&i| picker.items.get(i))
                    .and_then(|item| item.value.parse::<u64>().ok())
                    .map(conversation::ConversationId);
                if let Some(target) = target {
                    self.stop_background(target);
                }
            }
            events::PickerKind::Models => {
                if let Some(model) = indices
                    .first()
                    .and_then(|&i| picker.items.get(i))
                    .map(|item| item.value.clone())
                {
                    self.apply_model_switch(&model);
                }
            }
            _ => {
                if let Some(idx) = indices.first()
                    && let Some(item) = picker.items.get(*idx)
                {
                    let id = item.value.clone();
                    self.handle_load_session(&id).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_keys_decode() {
        for code in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            assert_eq!(approval_key(code), Some(ApprovalKey::Approve));
        }
        for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            assert_eq!(approval_key(code), Some(ApprovalKey::Deny));
        }
        assert_eq!(
            approval_key(KeyCode::Char('a')),
            Some(ApprovalKey::ToggleAlways)
        );
        assert_eq!(approval_key(KeyCode::Up), Some(ApprovalKey::ScrollUp));
        assert_eq!(approval_key(KeyCode::Down), Some(ApprovalKey::ScrollDown));
        assert_eq!(approval_key(KeyCode::Char('x')), None);
        assert_eq!(approval_key(KeyCode::Tab), None);
    }

    #[test]
    fn confirm_keys_decode() {
        for code in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert_eq!(confirm_key(code), Some(true));
        }
        for code in [KeyCode::Esc, KeyCode::Char('n'), KeyCode::Char('N')] {
            assert_eq!(confirm_key(code), Some(false));
        }
        assert_eq!(confirm_key(KeyCode::Char('q')), None);
        assert_eq!(confirm_key(KeyCode::Up), None);
    }
}
