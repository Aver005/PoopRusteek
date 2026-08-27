//! Multi-conversation management: `/btw` sidechats, sub-agents, and parallel
//! session switching. With the unified [`Conversations`](conversation::Conversations)
//! store there is no live/parked split — switching focus is just a focus change,
//! and every conversation (focused or not) is a full record.

use super::{App, conversation, generation};
use crate::app::events::AppEvent;
use crate::error::AppResult;
use crate::provider::{ChatMessage, Role};
use std::sync::Arc;

/// Fire-and-forget deletion of an ephemeral conversation's server-side
/// session — one-shot sidechat/sub-agent runs must not pile up junk chats on
/// the user's DeepSeek account. Detached: session cleanup must never block
/// the event loop, and a failure only means one leftover chat upstream.
fn discard_remote_session_of(conv: &conversation::Conversation) {
    if let Some(provider) = conv.provider.clone() {
        tokio::spawn(async move {
            if let Err(e) = provider.discard_remote_session().await {
                tracing::debug!("failed to delete ephemeral remote session: {e}");
            }
        });
    }
}

/// A short display title for a conversation, from its first user message.
fn conversation_title(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .chars()
                .take(40)
                .collect::<String>()
                .replace('\n', " ")
        })
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
        let session_id = crate::session::create_session_id();
        let messages = vec![ChatMessage::user(&prompt)];
        let system_prompt = super::system_prompt::build(
            &self.prompts,
            &self.skills,
            self.config.skills.injection,
            &self.tools,
            &self.mcp,
            self.config.effective_mcp_schema_mode(),
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
            tool_output_limit: self.config.context.tool_output_limit as usize,
            context: crate::context::ContextSpec::new(
                &self.config.context,
                self.state.provider_context_window,
                &session_id,
            )
            .with_output_cap(self.config.provider.max_tokens),
        });

        self.state
            .conversations
            .add_background(conversation::Conversation {
                id,
                kind,
                parent: Some(parent),
                title,
                session_id,
                session_started_at: chrono::Utc::now().to_rfc3339(),
                messages,
                provider: Some(provider),
                generation,
                agent_task: Some(handle),
                tag: None,
                broken: false,
                context_used: 0,
                compact_mode: None,
                compacting: None,
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
            .push(ChatMessage::system(&format!(
                "🤖 sub-agent started — {label}"
            )));
        Ok(())
    }

    /// Stop a running background conversation (sidechat / sub-agent) by id.
    pub(crate) fn stop_background(&mut self, target: conversation::ConversationId) {
        if let Some(mut conv) = self.state.conversations.remove(target) {
            if let Some(handle) = conv.agent_task.take() {
                handle.abort();
            }
            discard_remote_session_of(&conv);
            let title = if conv.title.is_empty() {
                "(background agent)".to_string()
            } else {
                conv.title
            };
            self.state
                .focused_mut()
                .messages
                .push(ChatMessage::system(&format!("⏹ Stopped: {title}")));
        }
    }

    /// Apply an agent event that targets a non-focused conversation. Streaming
    /// events mutate that conversation; terminal events finalize sidechats /
    /// sub-agents (which flush their result) while parked sessions just stop.
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
        discard_remote_session_of(&conv);
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
        if let Some(pid) = conv.parent
            && pid != focused_id
            && let Some(pconv) = self.state.conversations.get_mut(pid)
        {
            pconv.messages.push(ChatMessage::system(&block));
            self.state
                .focused_mut()
                .messages
                .push(ChatMessage::system(&format!(
                    "{label} finished in another chat — {}",
                    conv.title
                )));
            return;
        }

        self.state
            .focused_mut()
            .messages
            .push(ChatMessage::system(&block));
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
            tag: None,
            broken: false,
            context_used: 0,
            compact_mode: None,
            compacting: None,
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

    /// Cycle focus to the next (`+1`) / previous (`-1`) user chat by id order.
    /// Sidechats / sub-agents are excluded — same set as the `/chats` picker;
    /// they finalize-and-remove themselves, which focus must not race.
    pub(crate) fn cycle_focus(&mut self, dir: i64) {
        if self.state.modal.is_some() {
            return;
        }
        let ids = self.state.conversations.ordered_session_ids();
        if ids.len() <= 1 {
            return;
        }
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
                if conv.is_streaming() {
                    "  [streaming]"
                } else {
                    ""
                }
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
                    if c.title.is_empty() {
                        "(agent)".to_string()
                    } else {
                        c.title.clone()
                    },
                    if c.is_streaming() {
                        "  [running]"
                    } else {
                        "  [done]"
                    }
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

impl App {
    /// `/compact [1|2|3]` — run rung 3 by hand. Spawned, never awaited on the
    /// event loop (invariant 1): summarising is one model call per chunk.
    pub(crate) fn spawn_compaction(&mut self, requested: Option<u8>) {
        let conversation = self.state.conversations.focused_id();
        let Some(chat) = self.state.conversations.get(conversation) else {
            return;
        };
        // Both refusals guard the same thing: the task rewrites a snapshot of
        // the history, so nothing else may be appending to it meanwhile.
        if chat.compacting.is_some() {
            self.state.status_message =
                "Already compacting this chat — wait for it to finish.".to_string();
            return;
        }
        if chat.is_streaming() {
            self.state.status_message =
                "Cannot compact while the chat is streaming — stop the turn or wait.".to_string();
            return;
        }
        let Some(provider) = chat.provider.clone() else {
            self.state.status_message = "No provider configured — cannot compact.".to_string();
            return;
        };
        let mode = crate::context::modes::CompactMode::from_number(
            requested
                .or(chat.compact_mode)
                .unwrap_or_else(|| self.config.context.effective_compact_mode()),
        );
        let messages = chat.messages.clone();
        let budget = crate::context::spec::budget_from_config(
            &self.config,
            self.state.provider_context_window,
        );
        // No window means no budget to plan against; mode 1 needs one to judge
        // whether the opening turn is small enough to keep.
        let usable = budget.usable().unwrap_or(0);
        let model = self.config.active_model().to_string();
        let event_tx = self.event_tx.clone();
        let snapshot_len = messages.len();
        self.state.status_message = format!("Compacting with mode {}…", mode.number());
        let handle = tokio::spawn(async move {
            // Summarise on a throwaway fork, never on the chat's own handle: a
            // DeepSeek provider would otherwise write the summariser prompt and
            // its reply into the live server branch the user can open in the
            // web UI.
            let summariser = provider.fork();
            let outcome =
                crate::context::compact::compact(&summariser, &messages, mode, usable, &model)
                    .await;
            let _ = summariser.discard_remote_session().await;
            // A provider that keeps history server-side still holds the old,
            // uncompacted branch. Rewriting the local copy changes nothing until
            // the server is made to forget it, so re-seed from the summary.
            if outcome.is_ok() && provider.keeps_server_side_history() {
                let _ = provider.reset().await;
            }
            let (messages, status) = match outcome {
                Ok(done) => (
                    Some(done.messages),
                    format!(
                        "Compacted with mode {}: ~{} tokens instead of ~{} ({} model call(s))",
                        mode.number(),
                        done.after_tokens,
                        done.before_tokens,
                        done.calls
                    ),
                ),
                Err(reason) => (None, reason),
            };
            let _ = event_tx.send(AppEvent::CompactFinished {
                conversation,
                messages,
                status,
            });
        });
        if let Some(target) = self.state.conversations.get_mut(conversation) {
            if let Some(number) = requested {
                target.compact_mode = Some(number);
            }
            target.begin_compaction(snapshot_len, std::time::Instant::now());
            // Kept so Ctrl+C / Esc can abort the run; that path clears the flag
            // itself, because an aborted task sends no `CompactFinished`.
            target.agent_task = Some(handle);
        }
    }
}

impl App {
    /// `/compact` закончил. Итог вклеивается, а не отбрасывается: выжимка уже
    /// оплачена, и дописать пришедшее следом ничего не теряет.
    pub(crate) fn on_compact_finished(
        &mut self,
        conversation: conversation::ConversationId,
        messages: Option<Vec<crate::provider::ChatMessage>>,
        status: String,
    ) {
        let mut status = status;
        if let Some(target) = self.state.conversations.get_mut(conversation) {
            match (target.end_compaction(), messages) {
                (Some(base), Some(rebuilt)) => match target.swap_compacted(base, rebuilt) {
                    Some(0) => {}
                    Some(extra) => status = format!("{status}; kept {extra} newer message(s)"),
                    None => {
                        status = "Compaction dropped: this chat's history changed while it ran."
                            .to_string();
                    }
                },
                (Some(_), None) => {}
                // Флага нет — прогон отменили, его история заведомо устарела.
                (None, _) => status = "Compaction dropped: it was cancelled.".to_string(),
            }
        }
        if conversation == self.state.conversations.focused_id() {
            self.state.status_message = status;
        }
    }
}
