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
    /// Budget tokens the last request for this conversation carried, as
    /// measured by `crate::context::conversation_tokens`. 0 until the first
    /// step reports one. An estimate for thresholds, never a token count.
    pub context_used: u32,
    /// `/compact <n>` for this chat only. `None` falls back to
    /// `[context] compact_mode`.
    pub compact_mode: Option<u8>,
    /// Rung 3 in flight: how many messages the compaction task snapshotted.
    /// `None` when idle — set for the whole run, cleared on every exit path.
    pub compacting: Option<usize>,
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
            context_used: 0,
            compact_mode: None,
            compacting: None,
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

    // ─── Rung 3 (`/compact`) lifecycle ─────────────────────────
    // The busy flag doubles as the snapshot length, so the finishing side can
    // tell whether the history it is about to replace is still the one it read.

    /// Mark a compaction as started over `snapshot_len` messages. Busy is held
    /// on `generation` too, so the Enter guard blocks a turn meanwhile.
    pub fn begin_compaction(&mut self, snapshot_len: usize, now: std::time::Instant) {
        self.compacting = Some(snapshot_len);
        self.generation.begin(now);
    }

    /// Release the busy state and hand back the snapshot length. `None` means
    /// this run was already dropped (cancelled) and its result must be ignored.
    pub fn end_compaction(&mut self) -> Option<usize> {
        let base = self.compacting.take()?;
        self.generation.active = false;
        self.agent_task = None;
        Some(base)
    }

    /// Swap in a compacted history, keeping whatever arrived after the
    /// snapshot. `None` = the history shrank under the task; nothing is
    /// touched.
    pub fn swap_compacted(&mut self, base: usize, mut rebuilt: Vec<ChatMessage>) -> Option<usize> {
        let appended = self.messages.len().checked_sub(base)?;
        rebuilt.extend_from_slice(&self.messages[base..]);
        self.messages = rebuilt;
        self.context_used = crate::context::conversation_tokens("", &self.messages);
        Some(appended)
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

    fn chat(messages: &[&str]) -> Conversation {
        let mut conv = Conversation::fresh_main(None);
        conv.messages = messages.iter().map(|m| ChatMessage::user(m)).collect();
        conv
    }

    #[test]
    fn a_running_compaction_marks_the_chat_busy() {
        let mut conv = chat(&["a", "b"]);
        assert!(!conv.is_streaming());
        conv.begin_compaction(2, std::time::Instant::now());
        assert_eq!(conv.compacting, Some(2));
        // The Enter guard in `keys/chat.rs` reads exactly this flag.
        assert!(conv.is_streaming());
    }

    #[test]
    fn ending_a_compaction_releases_the_chat_once() {
        let mut conv = chat(&["a"]);
        conv.begin_compaction(1, std::time::Instant::now());
        assert_eq!(conv.end_compaction(), Some(1));
        assert!(!conv.is_streaming());
        assert_eq!(conv.compacting, None);
        // A second result for the same run finds no claim and must be ignored.
        assert_eq!(conv.end_compaction(), None);
    }

    #[test]
    fn messages_that_arrived_during_a_compaction_survive_the_swap() {
        let mut conv = chat(&["one", "two", "three"]);
        conv.begin_compaction(3, std::time::Instant::now());
        conv.messages.push(ChatMessage::user("sent meanwhile"));
        let base = conv.end_compaction().expect("claim held");
        let rebuilt = vec![ChatMessage::user("[Context summary] …")];

        assert_eq!(conv.swap_compacted(base, rebuilt), Some(1));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].content, "sent meanwhile");
    }

    #[test]
    fn a_history_that_shrank_is_never_overwritten() {
        let mut conv = chat(&["one", "two", "three"]);
        conv.begin_compaction(3, std::time::Instant::now());
        conv.messages.clear(); // Esc / a loaded session
        let base = conv.end_compaction().expect("claim held");

        assert_eq!(
            conv.swap_compacted(base, vec![ChatMessage::user("x")]),
            None
        );
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn a_swap_refreshes_the_context_indicator() {
        let mut conv = chat(&["a very long opening message"; 40]);
        conv.context_used = 9_999;
        conv.begin_compaction(40, std::time::Instant::now());
        let base = conv.end_compaction().expect("claim held");

        assert!(
            conv.swap_compacted(base, vec![ChatMessage::user("short summary")])
                .is_some()
        );
        assert_eq!(
            conv.context_used,
            crate::context::conversation_tokens("", &conv.messages)
        );
        assert!(conv.context_used < 9_999);
    }
}
