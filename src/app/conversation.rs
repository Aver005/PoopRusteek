//! Conversation identity.
//!
//! A conversation is one independent chat thread — the main chat, a `/btw`
//! sidechat, a parallel session, or a sub-agent. Each will own its messages,
//! a forked provider/session, generation status, and agent task (Phase 1).
//! A conversation is one independent chat thread; the *focused* one's live
//! state stays on `App`/`AppState`, while non-focused (background) ones are
//! parked here as [`Conversation`] records that keep streaming on their own
//! tasks.

use super::generation::GenerationState;
use crate::provider::{ChatMessage, LLMProvider};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Stable, process-unique id for a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConversationId(pub u64);

static NEXT_CONVERSATION_ID: AtomicU64 = AtomicU64::new(1);

impl ConversationId {
    /// Allocate the next unused conversation id.
    pub fn next() -> Self {
        ConversationId(NEXT_CONVERSATION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of conversation this is — drives titling, lifecycle, and how it is
/// surfaced in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    /// The primary chat the user starts in.
    Main,
    /// A user-opened parallel session (Phase 3).
    Session,
    /// A one-shot `/btw` side-answer (Phase 2).
    Sidechat,
    /// An isolated sub-agent run (Phase 4).
    SubAgent,
}

/// A parked (non-focused) conversation: it owns its own messages, forked
/// provider/session, generation status, and agent task, so it keeps streaming
/// independently while the user looks at something else.
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    /// For sub-agents/sidechats: the conversation that spawned it (where to
    /// notify / deliver the result). `None` for top-level chats.
    pub parent: Option<ConversationId>,
    pub title: String,
    pub session_id: String,
    pub session_started_at: String,
    pub messages: Vec<ChatMessage>,
    pub provider: Option<Arc<dyn LLMProvider>>,
    pub generation: GenerationState,
    pub agent_task: Option<tokio::task::JoinHandle<()>>,
    /// Mirrors the on-disk `Session.tag` so `auto_save_session` can round-trip
    /// it — without this, every autosave after a load/import silently wiped
    /// the tag back to `None` since it only ever wrote a fresh default.
    pub tag: Option<String>,
    /// Mirrors the on-disk `Session.broken` flag: true once this
    /// conversation's remote DeepSeek session was found unreachable. Drives
    /// the yellow flag in `/sessions` and is cleared again once a fresh
    /// remote session is established (see `App::auto_save_session`).
    pub broken: bool,
}

impl Conversation {
    /// Fresh, message-less main conversation — the state right after launch
    /// (and after `/logout` / `/wipe` or an onboarding submit).
    pub fn fresh_main(provider: Option<Arc<dyn LLMProvider>>) -> Self {
        Self {
            id: ConversationId::next(),
            kind: ConversationKind::Main,
            parent: None,
            title: String::new(),
            session_id: crate::session::create_session_id(),
            session_started_at: chrono::Utc::now().to_rfc3339(),
            messages: Vec::new(),
            provider,
            generation: GenerationState::default(),
            agent_task: None,
            tag: None,
            broken: false,
        }
    }

    /// Is this conversation's agent turn currently in flight?
    pub fn is_streaming(&self) -> bool {
        self.generation.active
    }

    /// Is this a background-kind conversation (sidechat / sub-agent) whose
    /// terminal events must finalize-and-flush regardless of focus?
    pub fn is_background_kind(&self) -> bool {
        matches!(
            self.kind,
            ConversationKind::Sidechat | ConversationKind::SubAgent
        )
    }

    // ─── Shared agent-event reducer ────────────────────────────
    // One implementation for both the focused and background event paths, so
    // streaming semantics can't drift between them.

    /// `BeginAssistantMessage`: open an empty assistant message unless one is
    /// already open.
    pub fn begin_assistant_message(&mut self) {
        let needs_push = self
            .messages
            .last()
            .is_none_or(|m| m.role != crate::provider::Role::Assistant || !m.content.is_empty());
        if needs_push {
            self.messages.push(ChatMessage::assistant(""));
        }
    }

    /// `AgentChunk`: append streamed content to the open assistant message.
    pub fn append_chunk(&mut self, chunk: &str) {
        if let Some(last) = self.messages.last_mut()
            && last.role == crate::provider::Role::Assistant
        {
            last.content.push_str(chunk);
        }
    }

    /// Drop a trailing assistant message that never received content.
    pub fn discard_empty_assistant(&mut self) {
        if self
            .messages
            .last()
            .is_some_and(|m| m.role == crate::provider::Role::Assistant && m.content.is_empty())
        {
            self.messages.pop();
        }
    }

    /// Terminal-event bookkeeping shared by `AgentDone` / `AgentError`.
    pub fn finish_turn(&mut self, status: &str) {
        self.generation.active = false;
        self.generation.last_status = Some(status.to_string());
        self.agent_task = None;
        self.discard_empty_assistant();
    }
}

/// The set of open conversations with one focused. There is no live/parked
/// split — every conversation, including the focused one, is a full
/// [`Conversation`] here, so switching focus is just changing an id and an
/// agent event is routed by looking the conversation up.
pub struct Conversations {
    items: Vec<Conversation>,
    focused: ConversationId,
}

impl Conversations {
    /// Create the store seeded with its first (focused) conversation.
    pub fn new(initial: Conversation) -> Self {
        let focused = initial.id;
        Self {
            items: vec![initial],
            focused,
        }
    }

    pub fn focused_id(&self) -> ConversationId {
        self.focused
    }

    pub fn focused(&self) -> &Conversation {
        self.items
            .iter()
            .find(|c| c.id == self.focused)
            .expect("focused conversation always exists")
    }

    pub fn focused_mut(&mut self) -> &mut Conversation {
        let focused = self.focused;
        self.items
            .iter_mut()
            .find(|c| c.id == focused)
            .expect("focused conversation always exists")
    }

    pub fn get_mut(&mut self, id: ConversationId) -> Option<&mut Conversation> {
        self.items.iter_mut().find(|c| c.id == id)
    }

    pub fn contains(&self, id: ConversationId) -> bool {
        self.items.iter().any(|c| c.id == id)
    }

    /// Focus an existing conversation (no-op if the id is unknown).
    pub fn set_focus(&mut self, id: ConversationId) {
        if self.contains(id) {
            self.focused = id;
        }
    }

    /// Add a conversation and focus it.
    pub fn open(&mut self, conv: Conversation) {
        self.focused = conv.id;
        self.items.push(conv);
    }

    /// Add a conversation without changing focus (sidechats / sub-agents).
    pub fn add_background(&mut self, conv: Conversation) {
        self.items.push(conv);
    }

    /// Remove a conversation. If it was focused, focus the lowest-id remaining
    /// one (the store never becomes empty in practice — the main chat stays).
    pub fn remove(&mut self, id: ConversationId) -> Option<Conversation> {
        let pos = self.items.iter().position(|c| c.id == id)?;
        let removed = self.items.remove(pos);
        if self.focused == id
            && let Some(first) = self.items.iter().map(|c| c.id).min_by_key(|c| c.0)
        {
            self.focused = first;
        }
        Some(removed)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Conversation> {
        self.items.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Conversation> {
        self.items.iter_mut()
    }

    // Not currently called; kept as the natural counterpart to `iter`/`get`
    // for callers that need a count or a stable cycling order. Not verified
    // dead against the same bar as the audited deletions elsewhere in this
    // pass, so annotated rather than removed.
    #[expect(
        dead_code,
        reason = "small accessor pair, not part of this pass's verified-dead list"
    )]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, id: ConversationId) -> Option<&Conversation> {
        self.items.iter().find(|c| c.id == id)
    }

    /// Conversation ids in stable (id) order — for cycling focus.
    #[expect(
        dead_code,
        reason = "small accessor pair, not part of this pass's verified-dead list"
    )]
    pub fn ordered_ids(&self) -> Vec<ConversationId> {
        let mut ids: Vec<ConversationId> = self.items.iter().map(|c| c.id).collect();
        ids.sort_by_key(|c| c.0);
        ids
    }

    /// Ids of user-facing chats only (main + parallel sessions), id-ordered.
    /// Tab cycles through these; sidechats/sub-agents live in `/agents`, and
    /// focusing one would race its own finalize-and-remove lifecycle.
    pub fn ordered_session_ids(&self) -> Vec<ConversationId> {
        let mut ids: Vec<ConversationId> = self
            .items
            .iter()
            .filter(|c| !c.is_background_kind())
            .map(|c| c.id)
            .collect();
        ids.sort_by_key(|c| c.0);
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_monotonic() {
        let a = ConversationId::next();
        let b = ConversationId::next();
        assert_ne!(a, b);
        assert!(b.0 > a.0);
    }
}
