use crate::app::conversation::ConversationId;
use crate::provider::ChatMessage;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

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

#[derive(Debug, Clone)]
pub struct QuestionRequest {
    answer: Arc<Mutex<Option<String>>>,
    notify: Arc<Notify>,
}

impl QuestionRequest {
    pub fn new() -> Self {
        Self {
            answer: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub async fn wait(&self) -> Option<String> {
        loop {
            {
                let answer = self.answer.lock().await;
                if answer.is_some() {
                    return answer.clone();
                }
            }
            self.notify.notified().await;
        }
    }

    pub async fn resolve(&self, value: String) {
        {
            let mut answer = self.answer.lock().await;
            *answer = Some(value);
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum GoalStage {
    #[default]
    Inactive,
    WaitForGoal,
    RunAgent1,
    RunEvaluator,
    Done,
}

#[derive(Debug, Clone)]
pub struct GoalVerdict {
    pub success: bool,
    pub summary: String,
    pub issues: String,
    pub feedback: String,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    // TUI events
    Key(crossterm::event::KeyEvent),
    Resize(u16, u16),
    Tick,

    // Agent events — each carries the conversation it belongs to so background
    // (sidechat / sub-agent) turns stream into the right place.
    AgentStarted(ConversationId),
    AgentChunk(ConversationId, String),
    AgentDone(ConversationId, AgentResult),
    AgentError(ConversationId, String),
    BeginAssistantMessage(ConversationId),
    DiscardEmptyAssistantMessage(ConversationId),
    AddMessage(ConversationId, ChatMessage),

    // Tool events
    ToolStarted { conversation: ConversationId, name: String },
    ToolDone { conversation: ConversationId, result: String },
    ToolError { conversation: ConversationId, error: String },
    RequestToolApproval(ToolApprovalRequest),
    RequestQuestion(QuestionRequest, QuestionState),

    // Goal events
    GoalEvaluationDone(GoalVerdict),
    GoalCycleFinished,

    /// A model `task` tool call asked for a detached sub-agent.
    SpawnSubAgent {
        parent: ConversationId,
        label: String,
        prompt: String,
    },
}

#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub tool_calls: Vec<ToolCallInfo>,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    pub tool_name: String,
    pub arguments: String,
    decision: Arc<Mutex<Option<bool>>>,
    notify: Arc<Notify>,
}

impl ToolApprovalRequest {
    pub fn new(tool_name: String, arguments: String) -> Self {
        Self {
            tool_name,
            arguments,
            decision: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub async fn wait(&self) -> bool {
        loop {
            {
                let decision = self.decision.lock().await;
                if let Some(value) = *decision {
                    return value;
                }
            }
            self.notify.notified().await;
        }
    }

    pub async fn resolve(&self, value: bool) {
        {
            let mut decision = self.decision.lock().await;
            *decision = Some(value);
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Chat,
    Mcp,
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub text: String,
    pub value: String,
}

impl PickerItem {
    pub fn new(text: impl Into<String>, value: impl Into<String>) -> Self {
        Self { text: text.into(), value: value.into() }
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

    pub fn new_with_kind(title: impl Into<String>, items: Vec<PickerItem>, mode: PickerMode, kind: PickerKind) -> Self {
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
        self.checked = self.items.iter()
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
            self.items = self.all_items.iter()
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
        crossterm::event::KeyCode::Enter => {
            match picker.mode {
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
            }
        }
        crossterm::event::KeyCode::Char(' ') => {
            match picker.mode {
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
            }
        }
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
}

pub fn handle_question_key(qs: &mut QuestionState, key: crossterm::event::KeyCode) -> Option<String> {
    match qs.options.is_empty() {
        true => match key {
            crossterm::event::KeyCode::Char('y') | crossterm::event::KeyCode::Char('Y') | crossterm::event::KeyCode::Enter =>
                Some("yes".to_string()),
            crossterm::event::KeyCode::Char('n') | crossterm::event::KeyCode::Char('N') | crossterm::event::KeyCode::Esc =>
                Some("no".to_string()),
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
                        if trimmed.is_empty() { None } else { Some(trimmed) }
                    }
                    crossterm::event::KeyCode::Backspace => {
                        if qs.custom_cursor > 0 {
                            qs.custom_cursor -= 1;
                            let byte_pos = qs.custom_input.char_indices()
                                .nth(qs.custom_cursor)
                                .map(|(i, _)| i)
                                .unwrap_or(qs.custom_input.len());
                            qs.custom_input.remove(byte_pos);
                        }
                        None
                    }
                    crossterm::event::KeyCode::Delete => {
                        let byte_pos = qs.custom_input.char_indices()
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
                    crossterm::event::KeyCode::Home => { qs.custom_cursor = 0; None }
                    crossterm::event::KeyCode::End => { qs.custom_cursor = qs.custom_input.chars().count(); None }
                    crossterm::event::KeyCode::Char(c) => {
                        let byte_pos = qs.custom_input.char_indices()
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
                    crossterm::event::KeyCode::Home => { qs.selected = 0; qs.update_scroll(); None }
                    crossterm::event::KeyCode::End => { qs.selected = qs.options.len().saturating_sub(1); qs.update_scroll(); None }
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
