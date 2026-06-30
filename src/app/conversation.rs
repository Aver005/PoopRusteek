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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
}

impl Conversation {
    /// Is this conversation's agent turn currently in flight?
    pub fn is_streaming(&self) -> bool {
        self.generation.active
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
