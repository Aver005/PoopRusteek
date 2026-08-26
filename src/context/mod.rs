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
pub mod prune;
pub mod spec;

/// How full the window must be before rung 1 starts clearing tool bodies.
/// Below the summary threshold on purpose: the cheap rung runs first.
pub const PRUNE_TRIGGER_PERCENT: u8 = 70;

/// How full the window must be before rung 2 resets a server-side session.
/// Above rung 1's trigger: clearing bodies is free, a re-seeded session is not.
pub const SESSION_RESET_PERCENT: u8 = 90;
mod tool_output;

pub use budget::{ContextBudget, budget_tokens, budget_tokens_for_chars, conversation_tokens};
pub use spec::ContextSpec;
pub use tool_output::cap_tool_output;
