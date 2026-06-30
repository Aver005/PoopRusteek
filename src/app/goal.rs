//! Goal-mode logic.
//!
//! Goal mode runs a two-agent loop (a worker and an evaluator) until the
//! evaluator reports success. The state lives in [`AppState`](super::AppState)
//! today, but the *pure* pieces — parsing the evaluator's verdict out of its
//! free-text reply — belong here where they can be tested in isolation.

use super::events::{GoalStage, GoalVerdict};

/// All state for goal mode, grouped out of the `AppState` god-object.
///
/// Goal mode drives a two-agent loop: `agent1` (the worker) attempts the goal,
/// then an evaluator judges it; on failure the worker retries with feedback.
/// Each agent gets swapped to a fresh session after repeated failures, which is
/// why the per-agent failure counters and session ids live here.
#[derive(Debug, Clone, Default)]
pub struct GoalState {
    pub mode: bool,
    pub stage: GoalStage,
    pub prompt: String,
    pub text: String,
    pub iteration: u32,
    pub agent1_failures: u32,
    pub agent2_failures: u32,
    pub agent1_session_id: String,
    pub agent2_session_id: String,
    pub summary: String,
}

/// Parse an evaluator reply into a structured [`GoalVerdict`].
///
/// The evaluator is asked to emit `**Status:**`, `**Summary:**`, `**Issues:**`
/// and `**Feedback:**` markers. As a fallback, any reply containing the word
/// `SUCCESS` with no structured summary is treated as success.
pub(crate) fn parse_goal_verdict(text: &str) -> GoalVerdict {
    let mut success = false;
    let mut summary = String::new();
    let mut issues = String::new();
    let mut feedback = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.contains("**Status:**") {
            success = trimmed.contains("SUCCESS");
        } else if trimmed.contains("**Summary:**") {
            summary = trimmed.splitn(2, "**Summary:**").nth(1).unwrap_or("").trim().to_string();
        } else if trimmed.contains("**Issues:**") {
            issues = trimmed.splitn(2, "**Issues:**").nth(1).unwrap_or("").trim().to_string();
        } else if trimmed.contains("**Feedback:**") {
            feedback = trimmed.splitn(2, "**Feedback:**").nth(1).unwrap_or("").trim().to_string();
        }
    }

    // Fallback: unstructured replies that mention SUCCESS count as success.
    if summary.is_empty() && text.contains("SUCCESS") {
        success = true;
    }

    GoalVerdict { success, summary, issues, feedback }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_success_with_summary() {
        let v = parse_goal_verdict("**Status:** SUCCESS\n**Summary:** all done");
        assert!(v.success);
        assert_eq!(v.summary, "all done");
    }

    #[test]
    fn structured_failure_with_issues_and_feedback() {
        let text = "**Status:** FAILURE\n**Issues:** tests fail\n**Feedback:** fix the parser";
        let v = parse_goal_verdict(text);
        assert!(!v.success);
        assert_eq!(v.issues, "tests fail");
        assert_eq!(v.feedback, "fix the parser");
    }

    #[test]
    fn unstructured_success_fallback() {
        // No structured fields, but the word SUCCESS appears.
        let v = parse_goal_verdict("Looks great, this is a SUCCESS overall.");
        assert!(v.success);
        assert!(v.summary.is_empty());
    }

    #[test]
    fn empty_reply_is_failure() {
        let v = parse_goal_verdict("");
        assert!(!v.success);
        assert!(v.summary.is_empty() && v.issues.is_empty() && v.feedback.is_empty());
    }

    #[test]
    fn structured_summary_suppresses_fallback() {
        // A summary is present, so the SUCCESS-word fallback must not fire;
        // the explicit FAILURE status stands.
        let v = parse_goal_verdict("**Status:** FAILURE\n**Summary:** partial — SUCCESS not reached");
        assert!(!v.success);
        assert_eq!(v.summary, "partial — SUCCESS not reached");
    }
}
