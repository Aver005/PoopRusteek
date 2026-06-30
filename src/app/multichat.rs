//! Multi-conversation management: `/btw` sidechats, sub-agents, and parallel
//! session switching. With the unified [`Conversations`](conversation::Conversations)
//! store there is no live/parked split — switching focus is just a focus change,
//! and every conversation (focused or not) is a full record.

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
        let provider = match self.state.focused().provider.as_ref() {
            Some(p) => p.fork(),
            None => {
                self.state.focused_mut().messages.push(ChatMessage::system(
                    "No provider configured. Set your DeepSeek token in config.",
                ));
                return Ok(());
            }
        };

        let id = conversation::ConversationId::next();
        let messages = vec![ChatMessage::user(&prompt)];
        let system_prompt = super::system_prompt::build(
            &self.prompts,
            &self.skills,
            &self.tools,
            &self.mcp,
            &self.state.workspace_path,
        )
        .await;

        let mut generation = generation::GenerationState::default();
        generation.begin(std::time::Instant::now());

        let handle = self.runtime.spawn(super::runtime::TurnSpec {
            conversation: id,
            provider: Arc::clone(&provider),
            messages: messages.clone(),
            system_prompt,
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature,
            max_tokens: self.config.provider.max_tokens,
            max_steps: self.config.agent.max_steps_per_turn.clamp(1, 8),
            max_tools_per_step: self.config.agent.max_tools_per_step.max(1),
            auto_approve: true, // background: auto-approve, never block on a modal
        });

        self.state.conversations.add_background(conversation::Conversation {
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
        let parent = self.state.conversations.focused_id();
        self.spawn_background_agent(
            conversation::ConversationKind::Sidechat,
            parent,
            question.clone(),
            question.clone(),
        )
        .await?;
        self.state
            .focused_mut()
            .messages
            .push(ChatMessage::system(&format!("◈ btw started — {question}")));
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
        self.state
            .focused_mut()
            .messages
            .push(ChatMessage::system(&format!("🤖 sub-agent started — {label}")));
        Ok(())
    }

    /// Stop a running background conversation (sidechat / sub-agent) by id.
    pub(crate) fn stop_background(&mut self, target: conversation::ConversationId) {
        if let Some(mut conv) = self.state.conversations.remove(target) {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
            let title = if conv.title.is_empty() { "(background agent)".to_string() } else { conv.title };
            self.state
                .focused_mut()
                .messages
                .push(ChatMessage::system(&format!("⏹ Stopped: {title}")));
        }
    }

    /// Apply an agent event that targets a non-focused conversation. Streaming
    /// events mutate that conversation; terminal events finalize sidechats /
    /// sub-agents (which flush their result) while parked sessions just stop.
    pub(crate) fn handle_background_event(
        &mut self,
        target: conversation::ConversationId,
        event: AppEvent,
    ) {
        enum Finish {
            None,
            Done,
            Error(String),
        }
        let mut finish = Finish::None;

        if let Some(conv) = self.state.conversations.get_mut(target) {
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
                _ => {}
            }
        }

        match finish {
            Finish::Done => self.finish_background(target, None),
            Finish::Error(err) => self.finish_background(target, Some(err)),
            Finish::None => {}
        }
    }

    /// Remove a finished sidechat / sub-agent and flush its result (answer or
    /// error) into the chat that spawned it (or the focused chat).
    pub(crate) fn finish_background(
        &mut self,
        target: conversation::ConversationId,
        error: Option<String>,
    ) {
        let Some(conv) = self.state.conversations.remove(target) else {
            return;
        };
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
        let focused_id = self.state.conversations.focused_id();
        if let Some(pid) = conv.parent {
            if pid != focused_id {
                if let Some(pconv) = self.state.conversations.get_mut(pid) {
                    pconv.messages.push(ChatMessage::system(&block));
                    self.state.focused_mut().messages.push(ChatMessage::system(&format!(
                        "{label} finished in another chat — {}",
                        conv.title
                    )));
                    return;
                }
            }
        }

        self.state.focused_mut().messages.push(ChatMessage::system(&block));
        self.auto_save_session();
    }

    /// Open a fresh parallel session and focus it; the current one keeps
    /// running in the background.
    pub(crate) fn new_conversation(&mut self) {
        if self.state.modal.is_some() {
            return;
        }
        let provider = self.state.focused().provider.as_ref().map(|p| p.fork());
        let has_provider = provider.is_some();
        self.state.conversations.open(conversation::Conversation {
            id: conversation::ConversationId::next(),
            kind: conversation::ConversationKind::Session,
            parent: None,
            title: String::new(),
            session_id: crate::session::create_session_id(),
            session_started_at: chrono::Utc::now().to_rfc3339(),
            messages: Vec::new(),
            provider,
            generation: generation::GenerationState::default(),
            agent_task: None,
        });
        self.state.scroll_offset = 0;
        self.state.status_message = if has_provider { "Ready" } else { "No provider" }.to_string();
        self.state.focused_mut().messages.push(ChatMessage::system(
            "New chat opened. /chats or Tab to switch between chats.",
        ));
    }

    /// Switch focus to another conversation (its stream keeps running).
    pub(crate) fn switch_to(&mut self, target: conversation::ConversationId) {
        if target == self.state.conversations.focused_id()
            || self.state.modal.is_some()
            || !self.state.conversations.contains(target)
        {
            return;
        }
        self.state.conversations.set_focus(target);
        self.state.scroll_offset = 0;
        self.state.status_message = if self.state.focused().generation.active {
            "Thinking...".to_string()
        } else {
            "Ready".to_string()
        };
    }

    /// Cycle focus to the next (`+1`) / previous (`-1`) conversation by id order.
    pub(crate) fn cycle_focus(&mut self, dir: i64) {
        if self.state.modal.is_some() || self.state.conversations.len() <= 1 {
            return;
        }
        let ids = self.state.conversations.ordered_ids();
        let focused = self.state.conversations.focused_id();
        let cur = ids.iter().position(|&i| i == focused).unwrap_or(0);
        let len = ids.len() as i64;
        let next = ids[(((cur as i64 + dir) % len + len) % len) as usize];
        self.switch_to(next);
    }

    /// Open the `/chats` picker listing parallel sessions (not sidechats /
    /// sub-agents); selecting one switches focus.
    pub(crate) async fn open_chats_picker(&mut self) {
        use crate::app::events::{Modal, PickerItem, PickerKind, PickerMode, PickerState};

        let focused = self.state.conversations.focused_id();
        let mut items: Vec<(conversation::ConversationId, PickerItem)> = Vec::new();
        for conv in self.state.conversations.iter() {
            // Sidechats / sub-agents live in `/agents`, not the chat switcher.
            if conv.kind == conversation::ConversationKind::Sidechat
                || conv.kind == conversation::ConversationKind::SubAgent
            {
                continue;
            }
            let marker = if conv.id == focused { "● " } else { "  " };
            let label = format!(
                "{marker}{}{}",
                conversation_title(&conv.messages),
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
            .conversations
            .iter()
            .filter(|c| {
                c.kind == conversation::ConversationKind::SubAgent
                    || c.kind == conversation::ConversationKind::Sidechat
            })
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
            self.state
                .focused_mut()
                .messages
                .push(ChatMessage::system("No background agents running."));
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
