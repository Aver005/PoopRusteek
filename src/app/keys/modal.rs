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

/// How many argument lines the tool-approval modal shows at once. The
/// keyboard scroll clamp here and the mouse-wheel clamp in `keys::mouse`
/// must agree, or one input method can scroll past the other's limit —
/// they used to be two independent magic `12`s.
pub(crate) const TOOL_APPROVAL_VISIBLE_LINES: usize = 12;

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
                grant,
                scope,
            } => {
                match approval_key(key.code) {
                    Some(ApprovalKey::Approve) => {
                        // Правило записывается ровно то, что подписано на
                        // экране: узкое по умолчанию, широкое — только если
                        // человек сам до него дощёлкал.
                        let rule = match grant {
                            events::Grant::Once => None,
                            events::Grant::Scope => scope
                                .clone()
                                .map(|s| crate::whitelist::Rule::scoped(&tool_name, s)),
                            events::Grant::Tool => Some(crate::whitelist::Rule::tool(&tool_name)),
                        };
                        // Состояние обновляем только если запись удалась:
                        // иначе разрешение живёт до перезапуска и исчезает.
                        if let Some(rule) = rule {
                            match crate::whitelist::persist(rule.clone()) {
                                Ok(()) => self.state.approved_tools.add(rule),
                                Err(error) => self.state.push_ui_system(&error),
                            }
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
                            grant: grant.next(scope.is_some()),
                            scope,
                        });
                    }
                    Some(ApprovalKey::ScrollUp) => {
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset: scroll_offset.saturating_sub(3),
                            grant,
                            scope,
                        });
                    }
                    Some(ApprovalKey::ScrollDown) => {
                        let max_scroll = arguments
                            .lines()
                            .count()
                            .saturating_sub(TOOL_APPROVAL_VISIBLE_LINES);
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset: (scroll_offset + 3).min(max_scroll),
                            grant,
                            scope,
                        });
                    }
                    None => {
                        self.state.modal = Some(Modal::ToolApproval {
                            tool_name,
                            arguments,
                            scroll_offset,
                            grant,
                            scope,
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
                    KeyCode::Char(c) if c != ' ' => {
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
                        events::ConfirmAction::Undo => self.spawn_undo(),
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
                let rules = indices
                    .iter()
                    .filter_map(|&i| picker.items.get(i))
                    .filter_map(|item| serde_json::from_str(&item.value).ok())
                    .collect();
                let list = crate::whitelist::Whitelist::from_rules(rules);
                if let Err(e) = crate::whitelist::save(&list) {
                    tracing::warn!("Failed to save whitelist: {e}");
                }
                self.state.approved_tools = list;
            }
            events::PickerKind::Skills => {
                let enabled: Vec<String> = indices
                    .iter()
                    .filter_map(|&i| picker.items.get(i))
                    .map(|item| item.value.clone())
                    .collect();
                // Save the in-memory config — reloading a fresh copy from
                // disk here used to silently clobber any other unsaved
                // config change made earlier in the session.
                self.config.skills.enabled = enabled.clone();
                if let Err(e) = crate::config::save(&self.config) {
                    tracing::warn!("Failed to save skills config: {e}");
                }
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
