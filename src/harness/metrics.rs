//! Turns a trace into numbers.
//!
//! Everything here is derived from the runner's own `debug_log` records, so
//! the metrics track what the agent actually did rather than what the
//! harness asked for. Several of those records are `key=value` message
//! strings rather than JSON payloads (they predate the JSONL sink), hence
//! [`fields`].

use crate::harness::trace::Trace;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How the turn ended, as the trace records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    /// `agent.turn.done` — a final answer was produced.
    Done,
    /// `agent.turn.error` with `status=max_steps_exceeded`.
    MaxSteps,
    /// The turn errored out (provider failure, stream timeout).
    Errored,
    /// Neither marker present: the process died or was aborted mid-turn.
    #[default]
    Unknown,
}

/// Per-run measurements.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunMetrics {
    pub steps: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    /// Calls dropped because the step's tool budget was already spent.
    pub tools_skipped: usize,
    pub tools_used: BTreeMap<String, usize>,
    /// How many times the model emitted an unparsable `<tool_use>` block and
    /// was asked to retry. The single most useful signal about prompt or
    /// parser regressions.
    pub malformed_tool_calls: usize,
    /// The retry budget ran out — the turn was abandoned over syntax.
    pub malformed_exhausted: bool,
    /// Streams that ended early but had a complete tool call to salvage.
    pub salvaged_streams: usize,
    pub stream_timeouts: usize,
    pub empty_assistant_steps: usize,
    /// A semantic hint was injected, and what it matched.
    pub semantic_hint: Option<String>,
    /// Characters of visible assistant text produced across all steps.
    pub visible_chars: usize,
    pub turn_end: TurnEnd,
    /// Non-zero means the trace file itself is damaged — treat the rest of
    /// these numbers as a lower bound.
    pub unparsable_lines: usize,
}

impl RunMetrics {
    pub fn from_trace(trace: &Trace) -> Self {
        let mut metrics = Self {
            unparsable_lines: trace.unparsable_lines,
            ..Self::default()
        };

        for record in &trace.records {
            match record.action.as_str() {
                "agent.step.start" => metrics.steps += 1,
                "agent.step.salvaged" => metrics.salvaged_streams += 1,
                "agent.step.empty_assistant" => metrics.empty_assistant_steps += 1,
                "agent.step.malformed_tool_use" => metrics.malformed_tool_calls += 1,
                "agent.step.malformed_tool_use_exhausted" => metrics.malformed_exhausted = true,
                "agent.tool.skipped" => metrics.tools_skipped += 1,
                "agent.step.error" => {
                    if message_field(record, "reason").as_deref() == Some("stream_timeout") {
                        metrics.stream_timeouts += 1;
                    }
                }
                "agent.semantic_hint" => {
                    metrics.semantic_hint = message_field(record, "matches");
                }
                "agent.tool.call.payload" => {
                    metrics.tool_calls += 1;
                    if let Some(name) = record.field("tool_name") {
                        *metrics.tools_used.entry(name.to_string()).or_default() += 1;
                    }
                }
                "agent.tool.result.payload" => {
                    let is_error = record
                        .data
                        .as_ref()
                        .and_then(|data| data.get("is_error"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if is_error {
                        metrics.tool_errors += 1;
                    }
                }
                "agent.step.parsed.payload" => {
                    metrics.visible_chars +=
                        record.number("visible_text_chars").unwrap_or(0) as usize;
                }
                "agent.turn.done" => metrics.turn_end = TurnEnd::Done,
                "agent.turn.error" => {
                    let status = message_field(record, "status");
                    metrics.turn_end = if status.as_deref() == Some("max_steps_exceeded") {
                        TurnEnd::MaxSteps
                    } else {
                        TurnEnd::Errored
                    };
                }
                _ => {}
            }
        }

        // Not every ending leaves an `agent.turn.*` marker: a provider error
        // sends `AgentError` straight out without one, so a 429 or a dead
        // endpoint would otherwise read as `unknown`. The driver's own
        // verdict fills the gap — no extra instrumentation in the agent loop,
        // which the harness is not allowed to touch.
        if metrics.turn_end == TurnEnd::Unknown {
            metrics.turn_end = match driver_status(trace) {
                Some("completed") => TurnEnd::Done,
                Some("failed" | "timed_out") => TurnEnd::Errored,
                _ if metrics.stream_timeouts > 0 => TurnEnd::Errored,
                _ => TurnEnd::Unknown,
            };
        }
        metrics
    }
}

/// The `status` the driver recorded when the run finished, if the trace has
/// that record at all (an aborted or killed child may not).
fn driver_status(trace: &Trace) -> Option<&str> {
    trace
        .by_action(crate::harness::trace::action::RUN_FINISHED)
        .last()?
        .field("status")
}

/// Pull `key=value` out of a space-separated message line.
///
/// A value runs to the next ` <ident>=` token, or to the end of the line —
/// not to the first space. Several producers put a whole sentence or a
/// comma-separated list in the last field (`errors=tool \`bash\`: … is not
/// valid JSON`, `matches=skill:a, tool:b`), and cutting those at the first
/// space reduced them to a single useless word.
pub fn message_field(record: &crate::harness::trace::TraceRecord, key: &str) -> Option<String> {
    let message = record.message.as_deref()?;
    let needle = format!("{key}=");
    let start = message.find(&needle)? + needle.len();
    let rest = &message[start..];
    Some(rest[..field_end(rest)].trim().to_string())
}

/// Byte offset where the current field's value stops. Every index used here
/// comes from `match_indices(' ')`, so it always lands on a char boundary.
fn field_end(rest: &str) -> usize {
    for (index, _) in rest.match_indices(' ') {
        let tail = &rest[index + 1..];
        let ident = tail.split(['=', ' ']).next().unwrap_or_default();
        let is_key = !ident.is_empty()
            && ident
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            && tail.as_bytes().get(ident.len()) == Some(&b'=');
        if is_key {
            return index;
        }
    }
    rest.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace_of(lines: &[&str]) -> Trace {
        Trace::parse(&lines.join("\n"))
    }

    #[test]
    fn counts_steps_tools_and_terminal_state() {
        let trace = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.step.start","message":"conversation=1 step=1/6"}"#,
            r#"{"seq":1,"ts":"t","action":"agent.tool.call.payload","data":{"tool_name":"bash"}}"#,
            r#"{"seq":2,"ts":"t","action":"agent.tool.result.payload","data":{"is_error":false}}"#,
            r#"{"seq":3,"ts":"t","action":"agent.tool.call.payload","data":{"tool_name":"bash"}}"#,
            r#"{"seq":4,"ts":"t","action":"agent.tool.result.payload","data":{"is_error":true}}"#,
            r#"{"seq":5,"ts":"t","action":"agent.step.parsed.payload","data":{"visible_text_chars":42}}"#,
            r#"{"seq":6,"ts":"t","action":"agent.turn.done","message":"conversation=1 status=success"}"#,
        ]);
        let metrics = RunMetrics::from_trace(&trace);
        assert_eq!(metrics.steps, 1);
        assert_eq!(metrics.tool_calls, 2);
        assert_eq!(metrics.tool_errors, 1);
        assert_eq!(metrics.tools_used.get("bash"), Some(&2));
        assert_eq!(metrics.visible_chars, 42);
        assert_eq!(metrics.turn_end, TurnEnd::Done);
    }

    #[test]
    fn recognizes_max_steps_and_malformed_calls() {
        let trace = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.step.malformed_tool_use","message":"conversation=1 retry=1/3"}"#,
            r#"{"seq":1,"ts":"t","action":"agent.step.malformed_tool_use_exhausted","message":"x"}"#,
            r#"{"seq":2,"ts":"t","action":"agent.turn.error","message":"conversation=1 status=max_steps_exceeded max_steps=6"}"#,
        ]);
        let metrics = RunMetrics::from_trace(&trace);
        assert_eq!(metrics.malformed_tool_calls, 1);
        assert!(metrics.malformed_exhausted);
        assert_eq!(metrics.turn_end, TurnEnd::MaxSteps);
    }

    #[test]
    fn driver_verdict_fills_in_a_missing_terminal_marker() {
        // A provider error (429, dead endpoint) never logs `agent.turn.*`.
        let failed = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.step.start","message":"conversation=1 step=1/3"}"#,
            r#"{"seq":1,"ts":"t","action":"harness.run.finished","data":{"status":"failed"}}"#,
        ]);
        assert_eq!(RunMetrics::from_trace(&failed).turn_end, TurnEnd::Errored);

        let timed_out = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"harness.run.finished","data":{"status":"timed_out"}}"#,
        ]);
        assert_eq!(
            RunMetrics::from_trace(&timed_out).turn_end,
            TurnEnd::Errored
        );

        // With no marker and no verdict there is genuinely nothing to say.
        let killed = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.step.start","message":"conversation=1 step=1/3"}"#,
        ]);
        assert_eq!(RunMetrics::from_trace(&killed).turn_end, TurnEnd::Unknown);
    }

    #[test]
    fn an_explicit_turn_marker_outranks_the_driver_verdict() {
        let trace = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.turn.error","message":"conversation=1 status=max_steps_exceeded"}"#,
            r#"{"seq":1,"ts":"t","action":"harness.run.finished","data":{"status":"completed"}}"#,
        ]);
        assert_eq!(RunMetrics::from_trace(&trace).turn_end, TurnEnd::MaxSteps);
    }

    #[test]
    fn stream_timeout_implies_error_without_a_terminal_marker() {
        let trace = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.step.error","message":"conversation=1 step=2/6 reason=stream_timeout"}"#,
        ]);
        let metrics = RunMetrics::from_trace(&trace);
        assert_eq!(metrics.stream_timeouts, 1);
        assert_eq!(metrics.turn_end, TurnEnd::Errored);
    }

    #[test]
    fn a_trailing_field_keeps_its_whole_sentence() {
        let trace = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.step.malformed_tool_use","message":"conversation=1 step=1/6 retry=1/2 errors=tool `bash`: <arguments> is not valid JSON (expected value at line 1 column 1)."}"#,
        ]);
        let record = &trace.records[0];
        let errors = message_field(record, "errors").unwrap();
        assert!(errors.contains("not valid JSON"), "{errors}");
        // Earlier fields still stop at the next key.
        assert_eq!(message_field(record, "retry").as_deref(), Some("1/2"));
        assert_eq!(message_field(record, "conversation").as_deref(), Some("1"));
    }

    #[test]
    fn message_fields_handle_lists_and_missing_keys() {
        let trace = trace_of(&[
            r#"{"seq":0,"ts":"t","action":"agent.semantic_hint","message":"conversation=1 matches=skill:a, tool:b"}"#,
        ]);
        let metrics = RunMetrics::from_trace(&trace);
        assert_eq!(metrics.semantic_hint.as_deref(), Some("skill:a, tool:b"));
        let record = &trace.records[0];
        assert!(message_field(record, "absent").is_none());
        assert_eq!(message_field(record, "conversation").as_deref(), Some("1"));
    }
}
