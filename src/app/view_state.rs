//! Pure state machines for the app's modal / full-screen UI: onboarding,
//! pickers, confirm dialogs, the `Modal` enum, the delete-sessions flow, and
//! the question prompt (plus their pure key reducers).
//!
//! Split out of `events.rs` (2026-07-15 quality audit): that file keeps the
//! cross-layer `AppEvent` vocabulary that provider-agnostic layers import;
//! everything here is UI state only the app/keys/tui triangle touches. It is
//! glob-re-exported through `app::events`, so historical
//! `use crate::app::events::{Modal, PickerState, …}` paths keep resolving —
//! new code may import from here directly.

use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct QuestionState {
    pub question: String,
    pub options: Vec<String>,
    pub allow_custom: bool,
    pub selected: usize,
    pub custom_input: String,
    pub custom_cursor: usize,
    pub is_custom_mode: bool,
    pub scroll_offset: usize,
}

impl QuestionState {
    pub fn new(question: String, options: Vec<String>, allow_custom: bool) -> Self {
        Self {
            question,
            options,
            allow_custom,
            selected: 0,
            custom_input: String::new(),
            custom_cursor: 0,
            is_custom_mode: false,
            scroll_offset: 0,
        }
    }
}

/// Pure state for the in-TUI onboarding screen. No app deps — fully testable.
#[derive(Debug, Clone, Default)]
pub struct OnboardingState {
    pub input: String,
    /// Char-indexed cursor (not byte offset).
    pub cursor: usize,
    /// When true the model selector shows deepseek-reasoner as active.
    pub model_reasoner: bool,
    /// Set after a failed submit; cleared on the next keypress.
    pub error: Option<&'static str>,
}

impl OnboardingState {
    /// Insert a char at the cursor position, advancing the cursor.
    pub fn insert(&mut self, ch: char) {
        self.error = None;
        let byte_pos = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.insert(byte_pos, ch);
        self.cursor += 1;
    }

    /// Delete the char immediately before the cursor (Backspace).
    pub fn backspace(&mut self) {
        self.error = None;
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let byte_pos = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.remove(byte_pos);
    }

    /// Toggle the model selector between deepseek-chat and deepseek-reasoner.
    pub fn toggle_model(&mut self) {
        self.model_reasoner = !self.model_reasoner;
    }

    /// Returns the model string for the selected option.
    pub fn model_str(&self) -> &'static str {
        if self.model_reasoner {
            "deepseek-reasoner"
        } else {
            "deepseek-chat"
        }
    }

    /// Validate and return the trimmed token, or set the error and return None.
    pub fn submit(&mut self) -> Option<String> {
        let token = self.input.trim().to_string();
        if token.is_empty() {
            self.error = Some("Token can't be empty — paste your userToken");
            None
        } else {
            Some(token)
        }
    }
}

#[cfg(test)]
mod onboarding_tests {
    use super::*;

    #[test]
    fn insert_and_backspace_ascii() {
        let mut s = OnboardingState::default();
        s.insert('a');
        s.insert('b');
        s.insert('c');
        assert_eq!(s.input, "abc");
        assert_eq!(s.cursor, 3);
        s.backspace();
        assert_eq!(s.input, "ab");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn insert_multibyte_emoji_boundary() {
        let mut s = OnboardingState::default();
        s.insert('🔑'); // 4 bytes
        s.insert('x');
        assert_eq!(s.input, "🔑x");
        assert_eq!(s.cursor, 2); // 2 chars
        s.backspace(); // removes 'x'
        assert_eq!(s.input, "🔑");
        assert_eq!(s.cursor, 1);
        s.backspace(); // removes emoji
        assert_eq!(s.input, "");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut s = OnboardingState::default();
        s.backspace();
        assert_eq!(s.input, "");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn toggle_model() {
        let mut s = OnboardingState::default();
        assert!(!s.model_reasoner);
        assert_eq!(s.model_str(), "deepseek-chat");
        s.toggle_model();
        assert!(s.model_reasoner);
        assert_eq!(s.model_str(), "deepseek-reasoner");
        s.toggle_model();
        assert!(!s.model_reasoner);
    }

    #[test]
    fn submit_empty_sets_error() {
        let mut s = OnboardingState::default();
        assert!(s.submit().is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn submit_trims_whitespace() {
        let mut s = OnboardingState::default();
        for c in "  tok123  \n".chars() {
            s.insert(c);
        }
        let result = s.submit();
        assert_eq!(result.as_deref(), Some("tok123"));
    }

    #[test]
    fn error_cleared_on_insert() {
        let mut s = OnboardingState::default();
        s.submit(); // sets error
        assert!(s.error.is_some());
        s.insert('x');
        assert!(s.error.is_none());
    }

    #[test]
    fn error_cleared_on_backspace() {
        let mut s = OnboardingState::default();
        s.insert('x');
        s.submit(); // won't error (non-empty), so force it
        // Re-test with the actual empty path
        let mut s2 = OnboardingState::default();
        s2.submit();
        assert!(s2.error.is_some());
        s2.backspace(); // noop on content but still clears error
        assert!(s2.error.is_none());
    }
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub text: String,
    pub value: String,
    /// Render this row flagged (e.g. yellow) — used by `/sessions` to mark
    /// sessions whose remote link was found dead.
    pub warn: bool,
}

impl PickerItem {
    pub fn new(text: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            value: value.into(),
            warn: false,
        }
    }

    pub fn warn(mut self, on: bool) -> Self {
        self.warn = on;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PickerMode {
    Single,
    Multi,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PickerKind {
    Sessions,
    Whitelist,
    Skills,
    Chats,
    Agents,
    /// `/models` — switch the active provider's model.
    Models,
}

#[derive(Debug, Clone)]
pub struct PickerState {
    pub title: String,
    pub all_items: Vec<PickerItem>,
    pub items: Vec<PickerItem>,
    pub checked: Vec<usize>,
    pub persistent_checked: Vec<String>,
    pub cursor: usize,
    pub scroll_offset: usize,
    pub mode: PickerMode,
    pub kind: PickerKind,
    pub search: String,
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>, mode: PickerMode) -> Self {
        let all_items = items.clone();
        Self {
            title: title.into(),
            all_items,
            items,
            checked: Vec::new(),
            persistent_checked: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            mode,
            kind: PickerKind::Sessions,
            search: String::new(),
        }
    }

    pub fn new_with_kind(
        title: impl Into<String>,
        items: Vec<PickerItem>,
        mode: PickerMode,
        kind: PickerKind,
    ) -> Self {
        let all_items = items.clone();
        Self {
            title: title.into(),
            all_items,
            items,
            checked: Vec::new(),
            persistent_checked: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            mode,
            kind,
            search: String::new(),
        }
    }

    pub fn sync_checked(&mut self) {
        let set: HashSet<&str> = self.persistent_checked.iter().map(|s| s.as_str()).collect();
        self.checked = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| set.contains(item.value.as_str()))
            .map(|(i, _)| i)
            .collect();
    }

    pub fn update_search(&mut self, query: String) {
        self.search = query;
        let q = self.search.to_lowercase();
        if q.is_empty() {
            self.items = self.all_items.clone();
        } else {
            self.items = self
                .all_items
                .iter()
                .filter(|item| item.text.to_lowercase().contains(&q))
                .cloned()
                .collect();
        }
        self.sync_checked();
        if self.cursor >= self.items.len() {
            self.cursor = self.items.len().saturating_sub(1);
        }
        if self.scroll_offset >= self.items.len() {
            self.scroll_offset = self.items.len().saturating_sub(1);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PickerAction {
    Selected(Vec<usize>),
    Cancelled,
    None,
}

pub fn handle_picker_key(picker: &mut PickerState, key: crossterm::event::KeyCode) -> PickerAction {
    const VISIBLE: usize = 12;
    match key {
        crossterm::event::KeyCode::Esc => PickerAction::Cancelled,
        crossterm::event::KeyCode::Enter => match picker.mode {
            PickerMode::Single => {
                if !picker.items.is_empty() {
                    PickerAction::Selected(vec![picker.cursor])
                } else {
                    PickerAction::Cancelled
                }
            }
            PickerMode::Multi => {
                let mut sel = picker.checked.clone();
                sel.sort();
                sel.dedup();
                PickerAction::Selected(sel)
            }
        },
        crossterm::event::KeyCode::Char(' ') => match picker.mode {
            PickerMode::Single => {
                if !picker.items.is_empty() {
                    PickerAction::Selected(vec![picker.cursor])
                } else {
                    PickerAction::None
                }
            }
            PickerMode::Multi => {
                let pos = picker.cursor;
                if let Some(item) = picker.items.get(pos) {
                    let v = &item.value;
                    if let Some(p) = picker.persistent_checked.iter().position(|x| x == v) {
                        picker.persistent_checked.remove(p);
                    } else {
                        picker.persistent_checked.push(v.clone());
                    }
                }
                picker.sync_checked();
                PickerAction::None
            }
        },
        crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
            picker.cursor = picker.cursor.saturating_sub(1);
            if picker.cursor < picker.scroll_offset {
                picker.scroll_offset = picker.cursor;
            }
            PickerAction::None
        }
        crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
            let max = picker.items.len().saturating_sub(1);
            picker.cursor = (picker.cursor + 1).min(max);
            if picker.cursor >= picker.scroll_offset + VISIBLE {
                picker.scroll_offset = picker.cursor + 1 - VISIBLE;
            }
            PickerAction::None
        }
        crossterm::event::KeyCode::Home => {
            picker.cursor = 0;
            picker.scroll_offset = 0;
            PickerAction::None
        }
        crossterm::event::KeyCode::End => {
            let max = picker.items.len().saturating_sub(1);
            picker.cursor = max;
            picker.scroll_offset = max.saturating_sub(VISIBLE - 1);
            PickerAction::None
        }
        _ => PickerAction::None,
    }
}

// ─── Generic confirm modal ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmAction {
    Logout,
    Wipe,
    /// `/update` invoked from a debug/`cargo run` build — confirm before
    /// replacing the running dev binary with the released one.
    Update,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmLineKind {
    Normal,
    Soft,
    Dim,
    Danger,
}

#[derive(Debug, Clone)]
pub struct ConfirmLine {
    pub text: String,
    pub kind: ConfirmLineKind,
}

impl ConfirmLine {
    pub fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: ConfirmLineKind::Normal,
        }
    }
    pub fn soft(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: ConfirmLineKind::Soft,
        }
    }
    pub fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: ConfirmLineKind::Dim,
        }
    }
    pub fn danger(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: ConfirmLineKind::Danger,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmState {
    pub title: String,
    pub lines: Vec<ConfirmLine>,
    pub action: ConfirmAction,
}

impl ConfirmState {
    pub fn logout() -> Self {
        Self {
            title: "Log out".to_string(),
            lines: vec![
                ConfirmLine::normal("Your DeepSeek token will be removed from config.toml."),
                ConfirmLine::dim("Local sessions, history and settings stay on disk."),
            ],
            action: ConfirmAction::Logout,
        }
    }

    pub fn update_dev() -> Self {
        Self {
            title: "Update from a dev build?".to_string(),
            lines: vec![
                ConfirmLine::normal(
                    "You're running a development build (debug, e.g. `cargo run`).",
                ),
                ConfirmLine::soft(
                    "Updating replaces the running binary with the released build from the `latest` GitHub release.",
                ),
                ConfirmLine::dim(
                    "Under `cargo run` that file lives in target/debug — cargo may keep running the downloaded release until the next source change or `cargo clean`.",
                ),
                ConfirmLine::danger("Are you sure you want to update?"),
            ],
            action: ConfirmAction::Update,
        }
    }

    pub fn wipe() -> Self {
        Self {
            title: "Wipe all local data".to_string(),
            lines: vec![
                ConfirmLine::normal("This deletes ALL Pooprusteek data on this machine:"),
                ConfirmLine::soft(
                    "  config.toml (incl. token) \u{00B7} sessions/ \u{00B7} history.json",
                ),
                ConfirmLine::soft("  mcp.json \u{00B7} whitelist.json"),
                ConfirmLine::dim(
                    "Foreign configs (.claude, .cursor, VS Code\u{2026}) are not touched.",
                ),
                ConfirmLine::danger("This action is irreversible."),
            ],
            action: ConfirmAction::Wipe,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Modal {
    ToolApproval {
        tool_name: String,
        arguments: String,
        scroll_offset: usize,
        always_allow: bool,
    },
    Picker(PickerState),
    Question(QuestionState),
    DeleteSessions(DeleteSessionsState),
    Confirm(ConfirmState),
    /// `/mcp add` — paste-JSON / step-by-step wizard for adding a new MCP
    /// server. State lives in `app::mcp_add` (its own module — see there).
    McpAdd(crate::app::mcp_add::McpAddState),
    /// `/providers add` — step-by-step wizard for adding an
    /// OpenAI-compatible provider. State lives in `app::providers`. Boxed
    /// for the same reason as `McpAdd`'s wizard: four `InputState` fields
    /// would otherwise size every `Modal`.
    ProviderAdd(Box<crate::app::providers::ProviderAddState>),
}

// ─── /delete — session deletion picker ────────────────────────────

/// Which sessions the delete picker shows — and, at confirm time, WHERE the
/// deletion applies: `All` removes both copies, `Local` only the on-disk
/// file, `Remote` only the account-side chat. `/delete` opens on `All`,
/// `/delete-local` on `Local`; Tab/arrows retarget freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    All,
    Local,
    Remote,
}

impl SessionScope {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Local,
            Self::Local => Self::Remote,
            Self::Remote => Self::All,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::All => Self::Remote,
            Self::Local => Self::All,
            Self::Remote => Self::Local,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Local => "Local",
            Self::Remote => "Remote",
        }
    }
}

/// One row in the delete picker: a session existing locally, remotely, or both.
#[derive(Debug, Clone)]
pub struct DeleteEntry {
    pub id: String,
    pub title: String,
    pub local: bool,
    pub remote: bool,
    /// Display timestamp (may be empty).
    pub updated_at: String,
}

/// Progress of the background remote-list fetch shown in the picker footer.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteListStatus {
    NoProvider,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeleteStage {
    Selecting,
    Confirming,
}

#[derive(Debug, Clone)]
pub struct DeleteSessionsState {
    pub entries: Vec<DeleteEntry>,
    pub filter: SessionScope,
    pub stage: DeleteStage,
    pub cursor: usize,
    pub scroll_offset: usize,
    /// Checked ids; survives filter switches, but a confirm only targets ids
    /// visible under the filter active at that moment — what you see is what
    /// you delete.
    pub checked: HashSet<String>,
    /// Targets captured when entering the confirm stage.
    pub confirm_ids: Vec<String>,
    pub remote_status: RemoteListStatus,
}

impl DeleteSessionsState {
    pub fn new(
        entries: Vec<DeleteEntry>,
        filter: SessionScope,
        remote_status: RemoteListStatus,
    ) -> Self {
        Self {
            entries,
            filter,
            stage: DeleteStage::Selecting,
            cursor: 0,
            scroll_offset: 0,
            checked: HashSet::new(),
            confirm_ids: Vec::new(),
            remote_status,
        }
    }

    /// Entries visible under the current filter, in stored order.
    pub fn visible(&self) -> Vec<&DeleteEntry> {
        self.entries
            .iter()
            .filter(|e| match self.filter {
                SessionScope::All => true,
                SessionScope::Local => e.local,
                SessionScope::Remote => e.remote,
            })
            .collect()
    }

    /// Merge the fetched remote list into the local-seeded entries (by id).
    pub fn merge_remote(&mut self, sessions: Vec<crate::provider::RemoteSessionInfo>) {
        for s in sessions {
            if let Some(entry) = self.entries.iter_mut().find(|e| e.id == s.id) {
                entry.remote = true;
                if entry.updated_at.is_empty() {
                    entry.updated_at = s.updated_at.unwrap_or_default();
                }
            } else {
                self.entries.push(DeleteEntry {
                    id: s.id,
                    title: s.title,
                    local: false,
                    remote: true,
                    updated_at: s.updated_at.unwrap_or_default(),
                });
            }
        }
        self.remote_status = RemoteListStatus::Ready;
    }

    /// The ids a confirm would target: checked entries visible under the
    /// current filter, or the highlighted one when nothing is checked.
    pub fn confirm_targets(&self) -> Vec<String> {
        let visible = self.visible();
        let checked: Vec<String> = visible
            .iter()
            .filter(|e| self.checked.contains(&e.id))
            .map(|e| e.id.clone())
            .collect();
        if !checked.is_empty() {
            return checked;
        }
        visible
            .get(self.cursor)
            .map(|e| vec![e.id.clone()])
            .unwrap_or_default()
    }
}

/// What the delete-picker key handler asks the app shell to do.
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteAction {
    None,
    Close,
    Execute {
        ids: Vec<String>,
        scope: SessionScope,
    },
}

pub fn handle_delete_sessions_key(
    state: &mut DeleteSessionsState,
    key: crossterm::event::KeyCode,
) -> DeleteAction {
    use crossterm::event::KeyCode;
    const VISIBLE: usize = 12;

    match state.stage {
        DeleteStage::Confirming => match key {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => DeleteAction::Execute {
                ids: std::mem::take(&mut state.confirm_ids),
                scope: state.filter,
            },
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                state.stage = DeleteStage::Selecting;
                state.confirm_ids.clear();
                DeleteAction::None
            }
            _ => DeleteAction::None,
        },
        DeleteStage::Selecting => match key {
            KeyCode::Esc | KeyCode::Char('q') => DeleteAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                state.cursor = state.cursor.saturating_sub(1);
                if state.cursor < state.scroll_offset {
                    state.scroll_offset = state.cursor;
                }
                DeleteAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = state.visible().len();
                if len > 0 && state.cursor + 1 < len {
                    state.cursor += 1;
                }
                if state.cursor >= state.scroll_offset + VISIBLE {
                    state.scroll_offset = state.cursor + 1 - VISIBLE;
                }
                DeleteAction::None
            }
            KeyCode::Char(' ') => {
                if let Some(id) = state.visible().get(state.cursor).map(|e| e.id.clone())
                    && !state.checked.remove(&id)
                {
                    state.checked.insert(id);
                }
                DeleteAction::None
            }
            KeyCode::Tab | KeyCode::Right => {
                state.filter = state.filter.next();
                state.cursor = 0;
                state.scroll_offset = 0;
                DeleteAction::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                state.filter = state.filter.prev();
                state.cursor = 0;
                state.scroll_offset = 0;
                DeleteAction::None
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Toggle select-all within the current filter view.
                let visible_ids: Vec<String> =
                    state.visible().iter().map(|e| e.id.clone()).collect();
                let all_checked = !visible_ids.is_empty()
                    && visible_ids.iter().all(|id| state.checked.contains(id));
                for id in visible_ids {
                    if all_checked {
                        state.checked.remove(&id);
                    } else {
                        state.checked.insert(id);
                    }
                }
                DeleteAction::None
            }
            KeyCode::Enter => {
                let targets = state.confirm_targets();
                if targets.is_empty() {
                    return DeleteAction::None;
                }
                state.confirm_ids = targets;
                state.stage = DeleteStage::Confirming;
                DeleteAction::None
            }
            _ => DeleteAction::None,
        },
    }
}

pub fn handle_question_key(
    qs: &mut QuestionState,
    key: crossterm::event::KeyCode,
) -> Option<String> {
    match qs.options.is_empty() {
        true => match key {
            crossterm::event::KeyCode::Char('y')
            | crossterm::event::KeyCode::Char('Y')
            | crossterm::event::KeyCode::Enter => Some("yes".to_string()),
            crossterm::event::KeyCode::Char('n')
            | crossterm::event::KeyCode::Char('N')
            | crossterm::event::KeyCode::Esc => Some("no".to_string()),
            _ => None,
        },
        false => {
            if qs.is_custom_mode {
                match key {
                    crossterm::event::KeyCode::Esc => {
                        qs.is_custom_mode = false;
                        qs.custom_input.clear();
                        qs.custom_cursor = 0;
                        None
                    }
                    crossterm::event::KeyCode::Enter => {
                        let trimmed = qs.custom_input.trim().to_string();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        }
                    }
                    crossterm::event::KeyCode::Backspace => {
                        if qs.custom_cursor > 0 {
                            qs.custom_cursor -= 1;
                            let byte_pos = qs
                                .custom_input
                                .char_indices()
                                .nth(qs.custom_cursor)
                                .map(|(i, _)| i)
                                .unwrap_or(qs.custom_input.len());
                            qs.custom_input.remove(byte_pos);
                        }
                        None
                    }
                    crossterm::event::KeyCode::Delete => {
                        let byte_pos = qs
                            .custom_input
                            .char_indices()
                            .nth(qs.custom_cursor)
                            .map(|(i, _)| i)
                            .unwrap_or(qs.custom_input.len());
                        if byte_pos < qs.custom_input.len() {
                            qs.custom_input.remove(byte_pos);
                        }
                        None
                    }
                    crossterm::event::KeyCode::Left => {
                        qs.custom_cursor = qs.custom_cursor.saturating_sub(1);
                        None
                    }
                    crossterm::event::KeyCode::Right => {
                        let max = qs.custom_input.chars().count();
                        qs.custom_cursor = (qs.custom_cursor + 1).min(max);
                        None
                    }
                    crossterm::event::KeyCode::Home => {
                        qs.custom_cursor = 0;
                        None
                    }
                    crossterm::event::KeyCode::End => {
                        qs.custom_cursor = qs.custom_input.chars().count();
                        None
                    }
                    crossterm::event::KeyCode::Char(c) => {
                        let byte_pos = qs
                            .custom_input
                            .char_indices()
                            .nth(qs.custom_cursor)
                            .map(|(i, _)| i)
                            .unwrap_or(qs.custom_input.len());
                        qs.custom_input.insert(byte_pos, c);
                        qs.custom_cursor += 1;
                        None
                    }
                    _ => None,
                }
            } else {
                match key {
                    crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
                        qs.selected = qs.selected.saturating_sub(1);
                        qs.update_scroll();
                        None
                    }
                    crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
                        let max = qs.options.len().saturating_sub(1);
                        qs.selected = (qs.selected + 1).min(max);
                        qs.update_scroll();
                        None
                    }
                    crossterm::event::KeyCode::Home => {
                        qs.selected = 0;
                        qs.update_scroll();
                        None
                    }
                    crossterm::event::KeyCode::End => {
                        qs.selected = qs.options.len().saturating_sub(1);
                        qs.update_scroll();
                        None
                    }
                    crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Char(' ') => {
                        if qs.allow_custom && qs.selected >= qs.options.len().saturating_sub(1) {
                            qs.is_custom_mode = true;
                            qs.custom_input.clear();
                            qs.custom_cursor = 0;
                            None
                        } else {
                            Some(qs.options[qs.selected].clone())
                        }
                    }
                    crossterm::event::KeyCode::Esc => Some(String::new()),
                    _ => None,
                }
            }
        }
    }
}

impl QuestionState {
    fn update_scroll(&mut self) {
        const VISIBLE: usize = 10;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + VISIBLE {
            self.scroll_offset = self.selected + 1 - VISIBLE;
        }
    }
}

#[cfg(test)]
mod delete_picker_tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn entry(id: &str, local: bool, remote: bool) -> DeleteEntry {
        DeleteEntry {
            id: id.to_string(),
            title: format!("title-{id}"),
            local,
            remote,
            updated_at: String::new(),
        }
    }

    fn state(filter: SessionScope) -> DeleteSessionsState {
        DeleteSessionsState::new(
            vec![
                entry("both", true, true),
                entry("loc", true, false),
                entry("rem", false, true),
            ],
            filter,
            RemoteListStatus::Ready,
        )
    }

    #[test]
    fn filter_controls_visibility() {
        let mut s = state(SessionScope::All);
        assert_eq!(s.visible().len(), 3);
        s.filter = SessionScope::Local;
        assert_eq!(
            s.visible()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["both", "loc"]
        );
        s.filter = SessionScope::Remote;
        assert_eq!(
            s.visible()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["both", "rem"]
        );
    }

    #[test]
    fn tab_cycles_filter_and_resets_cursor() {
        let mut s = state(SessionScope::All);
        s.cursor = 2;
        assert_eq!(
            handle_delete_sessions_key(&mut s, KeyCode::Tab),
            DeleteAction::None
        );
        assert_eq!(s.filter, SessionScope::Local);
        assert_eq!(s.cursor, 0);
        handle_delete_sessions_key(&mut s, KeyCode::Tab);
        assert_eq!(s.filter, SessionScope::Remote);
        handle_delete_sessions_key(&mut s, KeyCode::Tab);
        assert_eq!(s.filter, SessionScope::All);
        handle_delete_sessions_key(&mut s, KeyCode::BackTab);
        assert_eq!(s.filter, SessionScope::Remote);
    }

    #[test]
    fn space_toggles_check_and_enter_confirms_checked_visible_only() {
        let mut s = state(SessionScope::All);
        handle_delete_sessions_key(&mut s, KeyCode::Char(' ')); // check "both"
        handle_delete_sessions_key(&mut s, KeyCode::Down);
        handle_delete_sessions_key(&mut s, KeyCode::Char(' ')); // check "loc"
        assert!(s.checked.contains("both") && s.checked.contains("loc"));

        // Under the Remote filter only "both" of the checked set is visible —
        // confirm must target just that one (what you see is what you delete).
        s.filter = SessionScope::Remote;
        handle_delete_sessions_key(&mut s, KeyCode::Enter);
        assert_eq!(s.stage, DeleteStage::Confirming);
        assert_eq!(s.confirm_ids, vec!["both".to_string()]);

        // Execute binds the scope to the active filter.
        let action = handle_delete_sessions_key(&mut s, KeyCode::Enter);
        assert_eq!(
            action,
            DeleteAction::Execute {
                ids: vec!["both".to_string()],
                scope: SessionScope::Remote
            }
        );
    }

    #[test]
    fn enter_with_nothing_checked_targets_cursor_row() {
        let mut s = state(SessionScope::Local);
        handle_delete_sessions_key(&mut s, KeyCode::Down); // cursor -> "loc"
        handle_delete_sessions_key(&mut s, KeyCode::Enter);
        assert_eq!(s.confirm_ids, vec!["loc".to_string()]);
    }

    #[test]
    fn confirm_stage_backs_out_on_n_or_esc() {
        let mut s = state(SessionScope::All);
        handle_delete_sessions_key(&mut s, KeyCode::Enter);
        assert_eq!(s.stage, DeleteStage::Confirming);
        assert_eq!(
            handle_delete_sessions_key(&mut s, KeyCode::Char('n')),
            DeleteAction::None
        );
        assert_eq!(s.stage, DeleteStage::Selecting);
        assert!(s.confirm_ids.is_empty());
    }

    #[test]
    fn select_all_toggles_within_filter_view() {
        let mut s = state(SessionScope::Local);
        handle_delete_sessions_key(&mut s, KeyCode::Char('a'));
        assert!(
            s.checked.contains("both") && s.checked.contains("loc") && !s.checked.contains("rem")
        );
        handle_delete_sessions_key(&mut s, KeyCode::Char('a'));
        assert!(s.checked.is_empty());
    }

    #[test]
    fn merge_remote_marks_existing_and_appends_new() {
        let mut s = DeleteSessionsState::new(
            vec![
                entry("shared", true, false),
                entry("local-only", true, false),
            ],
            SessionScope::All,
            RemoteListStatus::Loading,
        );
        s.merge_remote(vec![
            crate::provider::RemoteSessionInfo {
                id: "shared".to_string(),
                title: "remote title".to_string(),
                updated_at: Some("2026-07-03T00:00:00Z".to_string()),
            },
            crate::provider::RemoteSessionInfo {
                id: "remote-only".to_string(),
                title: "fresh".to_string(),
                updated_at: None,
            },
        ]);
        assert_eq!(s.remote_status, RemoteListStatus::Ready);
        let shared = s.entries.iter().find(|e| e.id == "shared").unwrap();
        assert!(shared.local && shared.remote);
        let fresh = s.entries.iter().find(|e| e.id == "remote-only").unwrap();
        assert!(!fresh.local && fresh.remote);
        assert_eq!(s.entries.len(), 3);
    }

    #[test]
    fn escape_closes_from_selecting() {
        let mut s = state(SessionScope::All);
        assert_eq!(
            handle_delete_sessions_key(&mut s, KeyCode::Esc),
            DeleteAction::Close
        );
    }
}

#[cfg(test)]
mod confirm_tests {
    use super::*;

    #[test]
    fn logout_confirm_state() {
        let cs = ConfirmState::logout();
        assert_eq!(cs.title, "Log out");
        assert_eq!(cs.action, ConfirmAction::Logout);
        assert_eq!(cs.lines.len(), 2);
        assert_eq!(cs.lines[0].kind, ConfirmLineKind::Normal);
        assert_eq!(cs.lines[1].kind, ConfirmLineKind::Dim);
    }

    #[test]
    fn wipe_confirm_state() {
        let cs = ConfirmState::wipe();
        assert_eq!(cs.title, "Wipe all local data");
        assert_eq!(cs.action, ConfirmAction::Wipe);
        assert!(cs.lines.iter().any(|l| l.kind == ConfirmLineKind::Danger));
        assert_eq!(cs.lines[0].kind, ConfirmLineKind::Normal);
    }
}
