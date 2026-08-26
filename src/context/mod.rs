//! Context budget accounting — the measurement rung of the compaction ladder
//! described in `.docs/context-compaction.md`. Read that document before
//! extending this module: it records why the ladder is shaped this way and
//! which alternatives were rejected.
//!
//! `budget` only measures — it answers how full the window is, and answers
//! `None` whenever it does not know, so compaction degrades to off instead of
//! guessing (invariant 12). `tool_output` is rung 0 and does rewrite what the
//! model receives; the untrimmed text still reaches the UI and the trace.

pub mod budget;
mod tool_output;

pub use budget::{ContextBudget, conversation_tokens};
pub use tool_output::cap_tool_output;
