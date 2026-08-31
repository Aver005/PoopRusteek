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
        let needs_push = self.messages.last().is_none_or(|m| {
            m.role != crate::provider::Role::Assistant
                || !m.content.is_empty()
                || !m.tool_calls.is_empty()
        });
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

    /// Закрыть сообщение ассистента: прикрепить его вызовы и сбросить
    /// только пустышку.
    ///
    /// Сообщение с вызовами пустым не считается, даже когда текста в нём
    /// нет — а в родном протоколе это обычный вид шага. Сбросив его, мы
    /// оставили бы результаты инструментов без объявившего их вызова, и
    /// следующий ход провайдер отверг бы с 400.
    pub fn end_assistant_message(&mut self, tool_calls: &[crate::provider::ToolCall]) {
        // Ищем последнее сообщение ассистента, а не просто последнее: между
        // ним и этим событием цикл успевает вставить системную заметку
        // (обрезка, спасение вызова), и тогда вызовы прикрепились бы к ней —
        // то есть пропали бы вовсе.
        if let Some(index) = self
            .messages
            .iter()
            .rposition(|m| m.role == crate::provider::Role::Assistant)
            && self.messages[index].tool_calls.is_empty()
        {
            self.messages[index].tool_calls = tool_calls.to_vec();
        }
        self.discard_empty_assistant();
    }

    /// Закрыть вызовы, на которые не успели прийти результаты.
    ///
    /// Ход обрывают между объявлением вызовов и их исполнением — например,
    /// Esc на модалке подтверждения. Раньше такое сообщение было пустым и
    /// просто сбрасывалось; теперь оно несёт вызовы и остаётся, а вызов без
    /// результата провайдер отвергает. Дописываем явный результат вместо
    /// того, чтобы стирать сам факт попытки.
    pub fn settle_unanswered_tool_calls(&mut self) {
        let Some(index) = self
            .messages
            .iter()
            .rposition(|m| m.role == crate::provider::Role::Assistant && !m.tool_calls.is_empty())
        else {
            return;
        };
        let answered: std::collections::HashSet<String> = self.messages[index + 1..]
            .iter()
            .filter(|m| m.role == crate::provider::Role::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        let missing: Vec<(String, String)> = self.messages[index]
            .tool_calls
            .iter()
            .filter(|call| !answered.contains(&call.id))
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect();
        for (id, name) in missing {
            self.messages.push(ChatMessage::tool_with_display(
                &id,
                &name,
                "(the turn was interrupted before this tool ran)",
                "interrupted",
                true,
            ));
        }
    }

    /// Сбросить хвостовое сообщение ассистента, в которое не пришло ни
    /// текста, ни вызовов.
    pub fn discard_empty_assistant(&mut self) {
        if self.messages.last().is_some_and(|m| {
            m.role == crate::provider::Role::Assistant
                && m.content.is_empty()
                && m.tool_calls.is_empty()
        }) {
            self.messages.pop();
        }
    }

    /// Заменить очищенные ступенью 1 тела инструментов их маркерами.
    ///
    /// Только по id вызова, никогда по индексу: цикл агента работает на копии
    /// без `ui_only`-сообщений, так что списки не совпадают.
    pub fn clear_tool_output(&mut self, cleared: &[(String, String)]) {
        for (tool_call_id, marker) in cleared {
            if let Some(message) = self
                .messages
                .iter_mut()
                .find(|m| m.tool_call_id.as_deref() == Some(tool_call_id.as_str()))
            {
                message.content = marker.clone();
            }
        }
    }

    /// Terminal-event bookkeeping shared by `AgentEvent::Done` / `Failed`.
    pub fn finish_turn(&mut self, status: &str) {
        self.generation.active = false;
        self.generation.last_status = Some(status.to_string());
        self.agent_task = None;
        self.discard_empty_assistant();
        // Ход мог оборваться между объявлением вызовов и их исполнением.
        self.settle_unanswered_tool_calls();
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

    /// Шаг, состоящий из одних вызовов, текста не даёт — и именно такое
    /// сообщение раньше сбрасывалось. Результаты инструментов остались бы
    /// тогда без объявившего их вызова, а это 400 на следующем ходу.
    #[test]
    fn an_assistant_message_with_only_tool_calls_survives() {
        let mut conv = Conversation::fresh_main(None);
        conv.begin_assistant_message();
        conv.end_assistant_message(&[crate::provider::ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "x"}),
        }]);

        assert_eq!(conv.messages.len(), 1, "{:?}", conv.messages);
        assert_eq!(conv.messages[0].tool_calls.len(), 1);
        assert_eq!(conv.messages[0].tool_calls[0].id, "call_1");
    }

    /// А настоящая пустышка — ни текста, ни вызовов — сбрасывается, как и до
    /// сих пор.
    #[test]
    fn an_assistant_message_with_neither_text_nor_calls_is_dropped() {
        let mut conv = Conversation::fresh_main(None);
        conv.begin_assistant_message();
        conv.end_assistant_message(&[]);
        assert!(conv.messages.is_empty());
    }

    /// Следующий шаг открывает новое сообщение, а не дописывает текст в то,
    /// что уже объявило вызовы.
    #[test]
    fn a_new_assistant_message_opens_after_one_that_carried_calls() {
        let mut conv = Conversation::fresh_main(None);
        conv.begin_assistant_message();
        conv.end_assistant_message(&[crate::provider::ToolCall {
            id: "call_1".to_string(),
            name: "t".to_string(),
            arguments: serde_json::json!({}),
        }]);
        conv.begin_assistant_message();
        conv.append_chunk("the answer");

        assert_eq!(conv.messages.len(), 2);
        assert!(conv.messages[0].content.is_empty());
        assert_eq!(conv.messages[1].content, "the answer");
        assert!(conv.messages[1].tool_calls.is_empty());
    }

    fn a_call(id: &str) -> crate::provider::ToolCall {
        crate::provider::ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "x"}),
        }
    }

    /// Между открытием сообщения ассистента и его закрытием цикл успевает
    /// вставить системную заметку (обрезка, спасение вызова). Вызовы должны
    /// найти своё сообщение, а не прицепиться к заметке — иначе они
    /// пропадают, и результаты остаются без объявившего их вызова.
    #[test]
    fn calls_attach_past_a_system_notice_pushed_in_between() {
        let mut conv = Conversation::fresh_main(None);
        conv.begin_assistant_message();
        conv.messages
            .push(ChatMessage::system("Warning: stream ended early"));
        conv.end_assistant_message(&[a_call("call_1")]);

        let assistant = conv
            .messages
            .iter()
            .find(|m| m.role == crate::provider::Role::Assistant)
            .expect("the assistant message must survive");
        assert_eq!(assistant.tool_calls, vec![a_call("call_1")]);
    }

    /// Оборванный ход не оставляет вызова без ответа: провайдер такую
    /// историю отвергает, а обрывают её обычно на модалке подтверждения.
    #[test]
    fn an_interrupted_turn_answers_the_calls_it_left_hanging() {
        let mut conv = Conversation::fresh_main(None);
        conv.begin_assistant_message();
        conv.end_assistant_message(&[a_call("call_1"), a_call("call_2")]);
        conv.messages.push(ChatMessage::tool("call_1", "done"));

        conv.settle_unanswered_tool_calls();

        let answered: Vec<&str> = conv
            .messages
            .iter()
            .filter(|m| m.role == crate::provider::Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(answered, vec!["call_1", "call_2"]);
    }

    /// Повторный вызов ничего не дублирует: у каждого вызова ровно один
    /// результат.
    #[test]
    fn settling_twice_adds_nothing_the_second_time() {
        let mut conv = Conversation::fresh_main(None);
        conv.begin_assistant_message();
        conv.end_assistant_message(&[a_call("call_1")]);
        conv.settle_unanswered_tool_calls();
        let after_first = conv.messages.len();
        conv.settle_unanswered_tool_calls();
        assert_eq!(conv.messages.len(), after_first);
    }
}
