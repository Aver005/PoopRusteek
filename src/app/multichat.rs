//! Multi-conversation management: `/btw` sidechats, sub-agents, and parallel
//! session switching. These are `App` methods, split out of the large
//! `app/mod.rs`. Pure relocation — behavior is unchanged.

use super::{conversation, generation, App};
use crate::app::events::AppEvent;
use crate::error::AppResult;
use crate::provider::{ChatMessage, Role};
use std::sync::Arc;

/// A short display title for a conversation, from its first user message.
fn conversation_title(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.chars().take(40).collect::<String>().replace('\n', " "))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(empty chat)".to_string())
}

impl App {
    /// Spawn an isolated background agent (sidechat or sub-agent): a forked
    /// session that streams on its own task with auto-approved tools and
    /// reports back to `parent` when done.
    pub(crate) async fn spawn_background_agent(
        &mut self,
        kind: conversation::ConversationKind,
        parent: conversation::ConversationId,
        title: String,
        prompt: String,
    ) -> AppResult<()> {
        let provider = match &self.provider {
            Some(p) => p.fork(),
            None => {
                self.state.messages.push(ChatMessage::system(
                    "No provider configured. Set your DeepSeek token in config.",
                ));
                return Ok(());
            }
        };

        let id = conversation::ConversationId::next();
        let messages = vec![ChatMessage::user(&prompt)];
        let system_prompt = self.build_system_prompt().await;
        let model = self.config.provider.model.clone();
        let temperature = self.config.provider.temperature;
        let max_tokens = self.config.provider.max_tokens;
        let max_steps = self.config.agent.max_steps_per_turn.clamp(1, 8);
        let max_tools_per_step = self.config.agent.max_tools_per_step.max(1);
        let tools = Arc::clone(&self.tools);
        let mcp = Arc::clone(&self.mcp);
        let event_tx = self.event_tx.clone();

        let mut generation = generation::GenerationState::default();
        generation.begin(std::time::Instant::now());

        let handle = tokio::spawn(crate::agent::runner::run_agent_loop(
            id,
            Arc::clone(&provider),
            tools,
            mcp,
            messages.clone(),
            system_prompt,
            model,
            temperature,
            max_tokens,
            max_steps,
            max_tools_per_step,
            true, // background: auto-approve, never block on a modal
            event_tx,
        ));

        self.state.background.push(conversation::Conversation {
            id,
            kind,
            parent: Some(parent),
            title,
            session_id: crate::session::create_session_id(),
            session_started_at: chrono::Utc::now().to_rfc3339(),
            messages,
            provider: Some(provider),
            generation,
            agent_task: Some(handle),
        });
        Ok(())
    }

    /// `/btw` — one-shot side-question whose answer flushes into the chat.
    pub(crate) async fn spawn_sidechat(&mut self, question: String) -> AppResult<()> {
        let parent = self.state.focused_id;
        self.spawn_background_agent(
            conversation::ConversationKind::Sidechat,
            parent,
            question.clone(),
            question.clone(),
        )
        .await?;
        self.state.messages.push(ChatMessage::system(&format!("◈ btw started — {question}")));
        Ok(())
    }

    /// Spawn a tracked background sub-agent (from the model's `task` tool or
    /// `/agent`). Its result is delivered into the spawning chat when done.
    pub(crate) async fn spawn_sub_agent(
        &mut self,
        parent: conversation::ConversationId,
        label: String,
        prompt: String,
    ) -> AppResult<()> {
        self.spawn_background_agent(
            conversation::ConversationKind::SubAgent,
            parent,
            label.clone(),
            prompt,
        )
        .await?;
        self.state.messages.push(ChatMessage::system(&format!("🤖 sub-agent started — {label}")));
        Ok(())
    }

    /// Stop a running background conversation (sidechat / sub-agent) by id.
    pub(crate) fn stop_background(&mut self, target: conversation::ConversationId) {
        if let Some(pos) = self.state.background.iter().position(|c| c.id == target) {
            let mut conv = self.state.background.remove(pos);
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
            self.state.messages.push(ChatMessage::system(&format!(
                "⏹ Stopped: {}",
                if conv.title.is_empty() { "(background agent)" } else { &conv.title }
            )));
        }
    }

    /// Apply an agent event that targets a background conversation (not the
    /// focused one). Streaming events mutate the parked conversation; terminal
    /// events finalize it (sidechats flush their answer into the chat).
    pub(crate) fn handle_background_event(&mut self, target: conversation::ConversationId, event: AppEvent) {
        enum Finish {
            None,
            Done,
            Error(String),
        }
        let mut finish = Finish::None;

        if let Some(conv) = self.state.background.iter_mut().find(|c| c.id == target) {
            match event {
                AppEvent::AgentStarted(_) => {
                    conv.generation.begin(std::time::Instant::now());
                }
                AppEvent::BeginAssistantMessage(_) => {
                    let needs_push = conv
                        .messages
                        .last()
                        .is_none_or(|m| m.role != Role::Assistant || !m.content.is_empty());
                    if needs_push {
                        conv.messages.push(ChatMessage::assistant(""));
                    }
                }
                AppEvent::AgentChunk(_, chunk) => {
                    if let Some(last) = conv.messages.last_mut() {
                        if last.role == Role::Assistant {
                            last.content.push_str(&chunk);
                        }
                    }
                }
                AppEvent::AddMessage(_, message) => conv.messages.push(message),
                AppEvent::DiscardEmptyAssistantMessage(_) => {
                    if conv
                        .messages
                        .last()
                        .is_some_and(|m| m.role == Role::Assistant && m.content.is_empty())
                    {
                        conv.messages.pop();
                    }
                }
                AppEvent::AgentDone(_, _) => {
                    conv.generation.active = false;
                    conv.agent_task = None;
                    // Sidechats and sub-agents flush their result and disappear;
                    // a parked session just stops and waits to be switched to.
                    if conv.kind != conversation::ConversationKind::Session {
                        finish = Finish::Done;
                    }
                }
                AppEvent::AgentError(_, err) => {
                    conv.generation.active = false;
                    conv.agent_task = None;
                    if conv.kind != conversation::ConversationKind::Session {
                        finish = Finish::Error(err);
                    }
                }
                // Tool status from background turns is not surfaced live.
                _ => {}
            }
        }

        match finish {
            Finish::Done => self.finish_background(target, None),
            Finish::Error(err) => self.finish_background(target, Some(err)),
            Finish::None => {}
        }
    }

    /// Remove a finished background conversation (sidechat or sub-agent) and
    /// flush its result (answer or error) into the focused chat.
    pub(crate) fn finish_background(&mut self, target: conversation::ConversationId, error: Option<String>) {
        let Some(pos) = self.state.background.iter().position(|c| c.id == target) else {
            return;
        };
        let conv = self.state.background.remove(pos);
        let label = match conv.kind {
            conversation::ConversationKind::SubAgent => "🤖 sub-agent",
            _ => "◈ btw",
        };
        let block = if let Some(err) = error {
            format!("{label} — {}\n\n⚠ {err}", conv.title)
        } else {
            let answer = conv
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(no answer)".to_string());
            format!("{label} — {}\n\n{answer}", conv.title)
        };

        // Deliver into the chat that spawned it; if the user has switched away,
        // drop the result there and just notify the focused chat.
        if let Some(pid) = conv.parent {
            if pid != self.state.focused_id {
                let delivered = match self.state.background.iter_mut().find(|c| c.id == pid) {
                    Some(pconv) => {
                        pconv.messages.push(ChatMessage::system(&block));
                        true
                    }
                    None => false,
                };
                if delivered {
                    self.state.messages.push(ChatMessage::system(&format!(
                        "{label} finished in another chat — {}",
                        conv.title
                    )));
                    return;
                }
            }
        }

        self.state.messages.push(ChatMessage::system(&block));
        self.auto_save_session();
    }

    /// Pack the focused conversation's live state into a parked record.
    pub(crate) fn park_focused(&mut self) -> conversation::Conversation {
        conversation::Conversation {
            id: self.state.focused_id,
            kind: self.state.focused_kind,
            parent: None,
            title: conversation_title(&self.state.messages),
            session_id: std::mem::take(&mut self.state.current_session_id),
            session_started_at: std::mem::take(&mut self.state.session_started_at),
            messages: std::mem::take(&mut self.state.messages),
            provider: self.provider.take(),
            generation: std::mem::take(&mut self.state.generation),
            agent_task: self.agent_task.take(),
        }
    }

    /// Make a parked conversation the focused/live one (it keeps its running
    /// task — switching never drops a stream).
    pub(crate) fn activate(&mut self, conv: conversation::Conversation) {
        self.state.focused_id = conv.id;
        self.state.focused_kind = conv.kind;
        self.state.current_session_id = conv.session_id;
        self.state.session_started_at = conv.session_started_at;
        self.state.messages = conv.messages;
        self.provider = conv.provider;
        self.state.generation = conv.generation;
        self.agent_task = conv.agent_task;
        self.state.scroll_offset = 0;
        self.state.status_message = if self.state.generation.active {
            "Thinking...".to_string()
        } else {
            "Ready".to_string()
        };
    }

    /// Open a fresh parallel session and focus it; the current one keeps
    /// running in the background.
    pub(crate) fn new_conversation(&mut self) {
        if self.state.modal.is_some() {
            return;
        }
        let new_provider = self.provider.as_ref().map(|p| p.fork());
        let parked = self.park_focused();
        self.state.background.push(parked);

        self.state.focused_id = conversation::ConversationId::next();
        self.state.focused_kind = conversation::ConversationKind::Session;
        self.state.current_session_id = crate::session::create_session_id();
        self.state.session_started_at = chrono::Utc::now().to_rfc3339();
        self.state.messages = Vec::new();
        self.provider = new_provider;
        self.state.generation = generation::GenerationState::default();
        self.agent_task = None;
        self.state.scroll_offset = 0;
        self.state.status_message = if self.provider.is_some() {
            "Ready".to_string()
        } else {
            "No provider".to_string()
        };
        self.state.messages.push(ChatMessage::system(
            "New chat opened. /chats or Tab to switch between chats.",
        ));
    }

    /// Switch focus to a background conversation by id, parking the current one.
    pub(crate) fn switch_to(&mut self, target: conversation::ConversationId) {
        if target == self.state.focused_id || self.state.modal.is_some() {
            return;
        }
        let Some(pos) = self.state.background.iter().position(|c| c.id == target) else {
            return;
        };
        let parked = self.park_focused();
        let incoming = self.state.background.remove(pos);
        self.state.background.push(parked);
        self.activate(incoming);
    }

    /// Cycle focus to the next (`+1`) / previous (`-1`) conversation by id order.
    pub(crate) fn cycle_focus(&mut self, dir: i64) {
        if self.state.modal.is_some() || self.state.background.is_empty() {
            return;
        }
        let mut ids: Vec<conversation::ConversationId> =
            self.state.background.iter().map(|c| c.id).collect();
        ids.push(self.state.focused_id);
        ids.sort_by_key(|c| c.0);
        let cur = ids
            .iter()
            .position(|&i| i == self.state.focused_id)
            .unwrap_or(0);
        let len = ids.len() as i64;
        let next = ids[(((cur as i64 + dir) % len + len) % len) as usize];
        self.switch_to(next);
    }

    /// Open the `/chats` picker listing the focused conversation + background
    /// ones, with a streaming marker; selecting one switches focus.
    pub(crate) async fn open_chats_picker(&mut self) {
        use crate::app::events::{Modal, PickerItem, PickerKind, PickerMode, PickerState};

        let mut items: Vec<(conversation::ConversationId, PickerItem)> = Vec::new();
        let focused_label = format!(
            "● {}{}",
            conversation_title(&self.state.messages),
            if self.state.generation.active { "  [streaming]" } else { "" }
        );
        items.push((
            self.state.focused_id,
            PickerItem::new(focused_label, self.state.focused_id.0.to_string()),
        ));
        for conv in &self.state.background {
            // Sidechats / sub-agents live in `/agents`, not the chat switcher.
            if conv.kind != conversation::ConversationKind::Session {
                continue;
            }
            let label = format!(
                "  {}{}",
                if conv.title.is_empty() { "(chat)".to_string() } else { conv.title.clone() },
                if conv.is_streaming() { "  [streaming]" } else { "" }
            );
            items.push((conv.id, PickerItem::new(label, conv.id.0.to_string())));
        }
        items.sort_by_key(|(id, _)| id.0);

        let picker = PickerState::new_with_kind(
            "Chats — Enter to switch",
            items.into_iter().map(|(_, item)| item).collect(),
            PickerMode::Single,
            PickerKind::Chats,
        );
        self.state.modal = Some(Modal::Picker(picker));
    }

    /// Open the `/agents` picker listing running background sub-agents and
    /// sidechats; selecting one stops it.
    pub(crate) async fn open_agents_picker(&mut self) {
        use crate::app::events::{Modal, PickerItem, PickerKind, PickerMode, PickerState};

        let items: Vec<PickerItem> = self
            .state
            .background
            .iter()
            .filter(|c| c.kind != conversation::ConversationKind::Session)
            .map(|c| {
                let tag = match c.kind {
                    conversation::ConversationKind::SubAgent => "🤖",
                    _ => "◈",
                };
                let label = format!(
                    "{tag} {}{}",
                    if c.title.is_empty() { "(agent)".to_string() } else { c.title.clone() },
                    if c.is_streaming() { "  [running]" } else { "  [done]" }
                );
                PickerItem::new(label, c.id.0.to_string())
            })
            .collect();

        if items.is_empty() {
            self.state.messages.push(ChatMessage::system("No background agents running."));
            return;
        }

        let picker = PickerState::new_with_kind(
            "Agents — Enter to stop",
            items,
            PickerMode::Single,
            PickerKind::Agents,
        );
        self.state.modal = Some(Modal::Picker(picker));
    }
}
