//! Conversation identity.
//!
//! A conversation is one independent chat thread — the main chat, a `/btw`
//! sidechat, a parallel session, or a sub-agent. Each will own its messages,
//! a forked provider/session, generation status, and agent task (Phase 1).
//! For now this module provides the identity and kind so the rest of the code
//! can start routing agent events by conversation.
//!
// Phase 0 scaffolding: these are consumed starting in Phase 1 (the Conversation
// abstraction). The allow is narrowed/removed there.
#![allow(dead_code)]

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
