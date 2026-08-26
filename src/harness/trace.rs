//! Reading side of the turn trace: the JSONL lines `debug_log` writes when
//! configured with [`crate::debug_log::Format::Jsonl`]. One trace is one
//! agent turn (plus any sub-agents it spawned).
//!
//! The record envelope is deliberately loose — `action` is a dotted string
//! and `data` an untyped value — because the producers are the existing
//! `debug_log` call sites scattered through `agent::runner`. Analysis code
//! pulls typed views out of it (see [`Trace`] accessors); nothing here
//! fails on an unknown action, so adding instrumentation upstream never
//! breaks a consumer.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Actions the harness itself emits, to keep the string literals in one
/// place. Runner-side actions (`agent.*`) live at their call sites.
pub mod action {
    pub const RUN_STARTED: &str = "harness.run.started";
    pub const RUN_FINISHED: &str = "harness.run.finished";
    pub const SEMANTIC: &str = "harness.semantic";
    pub const APPROVAL: &str = "harness.approval";
    pub const QUESTION: &str = "harness.question";
    pub const SUB_AGENT: &str = "harness.sub_agent";
    pub const MESSAGE: &str = "harness.message";
    pub const TOOL_RESULT: &str = "harness.tool.result";
    pub const CONTEXT_WINDOW: &str = "harness.context.window";
    pub const TOOL_OUTPUT_CLEARED: &str = "harness.context.tool_output_cleared";
}

/// One line of a trace file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub seq: u64,
    pub ts: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl TraceRecord {
    /// `data.<key>` as a string, for the many single-field lookups analysis
    /// code does.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.data.as_ref()?.get(key)?.as_str()
    }

    pub fn number(&self, key: &str) -> Option<u64> {
        self.data.as_ref()?.get(key)?.as_u64()
    }
}

/// A parsed trace file.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub records: Vec<TraceRecord>,
    /// Lines that were not valid JSON. Non-zero means a truncated or
    /// interleaved file — worth reporting rather than silently averaging
    /// over, since it usually means the process died mid-write.
    pub unparsable_lines: usize,
}

impl Trace {
    pub fn read(path: &Path) -> AppResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AppError::Custom(format!("{}: {e}", path.display())))?;
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Self {
        let mut trace = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TraceRecord>(line) {
                Ok(record) => trace.records.push(record),
                Err(_) => trace.unparsable_lines += 1,
            }
        }
        trace.records.sort_by_key(|record| record.seq);
        trace
    }

    pub fn by_action<'a>(&'a self, action: &'a str) -> impl Iterator<Item = &'a TraceRecord> {
        self.records
            .iter()
            .filter(move |record| record.action == action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders_records_skipping_garbage() {
        let text = concat!(
            r#"{"seq":2,"ts":"t","action":"b","data":{"n":7}}"#,
            "\n",
            "not json at all\n",
            "\n",
            r#"{"seq":1,"ts":"t","action":"a","message":"hi"}"#,
            "\n",
        );
        let trace = Trace::parse(text);
        assert_eq!(trace.unparsable_lines, 1);
        assert_eq!(trace.records.len(), 2);
        // Sorted by seq, not file order.
        assert_eq!(trace.records[0].action, "a");
        assert_eq!(trace.records[0].message.as_deref(), Some("hi"));
        assert_eq!(trace.records[1].number("n"), Some(7));
    }

    #[test]
    fn by_action_filters_and_is_total_for_unknown_actions() {
        let trace = Trace::parse(concat!(
            r#"{"seq":0,"ts":"t","action":"x","message":"one"}"#,
            "
",
            r#"{"seq":1,"ts":"t","action":"y"}"#,
            "
",
            r#"{"seq":2,"ts":"t","action":"x","message":"two"}"#,
        ));
        assert_eq!(trace.by_action("x").count(), 2);
        assert_eq!(trace.by_action("missing").count(), 0);
    }
}
