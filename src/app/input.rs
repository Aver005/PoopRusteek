//! Text-input editing state.
//!
//! The prompt line's editable buffer, cursor, selection, and recall history
//! were previously six loose fields on the `AppState` god-object. Grouping
//! them here makes the input subsystem a cohesive unit and a future home for
//! the editing operations (insert, delete, cursor movement, history recall)
//! that currently live inline in the key handler.

/// Editable state of the prompt input line.
#[derive(Debug, Default)]
pub struct InputState {
    /// Current text in the prompt.
    pub buffer: String,
    /// Cursor position as a byte offset into `buffer`.
    pub cursor: usize,
    /// Anchor of an active selection, if any (byte offset into `buffer`).
    pub selection_anchor: Option<usize>,
    /// Previously submitted inputs, for up/down recall.
    pub history: Vec<String>,
    /// Position within `history` while recalling; `None` when editing fresh.
    pub history_index: Option<usize>,
    /// Buffer stashed when the user starts browsing history, restored on exit.
    pub unsent: String,
}
