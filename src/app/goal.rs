//! Goal-mode logic.
//!
//! Goal mode runs a two-agent loop (a worker and an evaluator) until the
//! evaluator reports success. The state lives in [`AppState`](super::AppState)
//! today, but the *pure* pieces — parsing the evaluator's verdict out of its
//! free-text reply — belong here where they can be tested in isolation.

use super::events::{AppEvent, GoalStage, GoalVerdict};
use super::App;
use crate::provider::{ChatMessage, Role};
use std::sync::Arc;

/// Stop the worker/evaluator loop after this many attempts so a goal that can
/// never be satisfied (or a flaky evaluator) doesn't retry forever.
pub(crate) const MAX_GOAL_ITERATIONS: u32 = 10;

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

/// What the imperative shell must surface after a verdict is applied.
///
/// `apply_verdict` performs all the goal *state* changes (counters, stage,
/// session swaps); the caller is left to do the I/O — pushing chat messages,
/// saving sessions, kicking off the retry.
pub(crate) enum GoalOutcome {
    /// Goal met. Display this summary.
    Succeeded { summary: String },
    /// Hit the attempt cap without success — stop and tell the user.
    GaveUp { iteration: u32 },
    /// Goal not met. Retry agent 1 with this feedback.
    Retry {
        /// Attempt number to display (the pre-increment iteration).
        iteration: u32,
        issues: String,
        feedback: String,
        /// A fresh agent-1 (worker) session was started this round.
        swapped_agent1: bool,
        /// A fresh agent-2 (evaluator) session was started this round.
        swapped_agent2: bool,
    },
}

impl GoalState {
    /// Apply an evaluator verdict: advance failure counters, swap a stale
    /// agent's session (worker after 3 failures, evaluator after 5), bump the
    /// iteration, and set the next stage. Returns what the shell should do.
    ///
    /// This is the goal loop's decision core, shared by the inline and
    /// event-driven verdict paths so the rules live in exactly one place.
    pub(crate) fn apply_verdict(&mut self, verdict: GoalVerdict) -> GoalOutcome {
        if verdict.success {
            let summary = if verdict.summary.is_empty() {
                "Goal achieved!".to_string()
            } else {
                verdict.summary
            };
            self.summary = summary.clone();
            self.stage = GoalStage::Done;
            return GoalOutcome::Succeeded { summary };
        }

        // Evaluator (agent 2): increment first, then swap if it keeps failing.
        self.agent2_failures += 1;
        let iteration = self.iteration;
        let swapped_agent2 = self.agent2_failures >= 5;
        if swapped_agent2 {
            self.agent2_session_id = crate::session::create_session_id();
            self.agent2_failures = 0;
        }

        // Stop retrying once we've made enough attempts.
        if iteration >= MAX_GOAL_ITERATIONS {
            self.stage = GoalStage::Done;
            return GoalOutcome::GaveUp { iteration };
        }

        let feedback = if verdict.feedback.is_empty() {
            "No specific feedback provided. Please re-examine the goal and fix remaining issues."
                .to_string()
        } else {
            verdict.feedback
        };

        self.iteration += 1;

        // Worker (agent 1): swap before counting this round's failure.
        let swapped_agent1 = self.agent1_failures >= 3;
        if swapped_agent1 {
            self.agent1_session_id = crate::session::create_session_id();
            self.agent1_failures = 0;
        }
        self.agent1_failures += 1;
        self.stage = GoalStage::RunAgent1;

        GoalOutcome::Retry {
            iteration,
            issues: verdict.issues,
            feedback,
            swapped_agent1,
            swapped_agent2,
        }
    }

    /// Is a worker/evaluator turn supposed to be in flight right now? When this
    /// is true but no agent is actually running (e.g. after an abort or a
    /// missing provider), the pipeline is wedged — callers use this to detect
    /// and recover from that.
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.stage, GoalStage::RunAgent1 | GoalStage::RunEvaluator)
    }

    /// Enter goal mode fresh: clear all per-run state and start two new agent
    /// sessions. Stage stays `Inactive` until the user supplies a prompt.
    pub(crate) fn activate(&mut self) {
        *self = GoalState {
            mode: true,
            agent1_session_id: crate::session::create_session_id(),
            agent2_session_id: crate::session::create_session_id(),
            ..GoalState::default()
        };
    }

    /// Leave goal mode and clear all per-run state. Safe to call at any time —
    /// it always lands in a non-wedged, fully-reset state.
    pub(crate) fn deactivate(&mut self) {
        *self = GoalState::default();
    }
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

    fn verdict(success: bool, summary: &str, issues: &str, feedback: &str) -> GoalVerdict {
        GoalVerdict {
            success,
            summary: summary.to_string(),
            issues: issues.to_string(),
            feedback: feedback.to_string(),
        }
    }

    #[test]
    fn apply_success_sets_done_and_summary() {
        let mut g = GoalState::default();
        let outcome = g.apply_verdict(verdict(true, "shipped it", "", ""));
        assert_eq!(g.stage, GoalStage::Done);
        assert_eq!(g.summary, "shipped it");
        match outcome {
            GoalOutcome::Succeeded { summary } => assert_eq!(summary, "shipped it"),
            _ => panic!("expected Succeeded"),
        }
    }

    #[test]
    fn apply_success_empty_summary_defaults() {
        let mut g = GoalState::default();
        let outcome = g.apply_verdict(verdict(true, "", "", ""));
        match outcome {
            GoalOutcome::Succeeded { summary } => assert_eq!(summary, "Goal achieved!"),
            _ => panic!("expected Succeeded"),
        }
    }

    #[test]
    fn apply_failure_advances_iteration_and_counters() {
        let mut g = GoalState::default();
        let outcome = g.apply_verdict(verdict(false, "", "tests red", "fix tests"));
        assert_eq!(g.stage, GoalStage::RunAgent1);
        assert_eq!(g.iteration, 1);
        assert_eq!(g.agent1_failures, 1);
        assert_eq!(g.agent2_failures, 1);
        match outcome {
            GoalOutcome::Retry { iteration, issues, feedback, swapped_agent1, swapped_agent2 } => {
                assert_eq!(iteration, 0); // pre-increment attempt number
                assert_eq!(issues, "tests red");
                assert_eq!(feedback, "fix tests");
                assert!(!swapped_agent1 && !swapped_agent2);
            }
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn apply_failure_empty_feedback_defaults() {
        let mut g = GoalState::default();
        let outcome = g.apply_verdict(verdict(false, "", "", ""));
        match outcome {
            GoalOutcome::Retry { feedback, .. } => assert!(feedback.starts_with("No specific feedback")),
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn evaluator_swaps_after_fifth_failure() {
        let mut g = GoalState { agent2_failures: 4, ..Default::default() };
        let outcome = g.apply_verdict(verdict(false, "", "", ""));
        // 4 + 1 == 5 → swap and reset the evaluator session.
        assert_eq!(g.agent2_failures, 0);
        assert!(!g.agent2_session_id.is_empty());
        match outcome {
            GoalOutcome::Retry { swapped_agent2, .. } => assert!(swapped_agent2),
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn worker_swaps_when_already_at_three_failures() {
        let mut g = GoalState { agent1_failures: 3, ..Default::default() };
        let outcome = g.apply_verdict(verdict(false, "", "", ""));
        // Check is before increment: swap, reset to 0, then count this round → 1.
        assert_eq!(g.agent1_failures, 1);
        assert!(!g.agent1_session_id.is_empty());
        match outcome {
            GoalOutcome::Retry { swapped_agent1, .. } => assert!(swapped_agent1),
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn gives_up_at_iteration_cap() {
        let mut g = GoalState { iteration: MAX_GOAL_ITERATIONS, ..Default::default() };
        match g.apply_verdict(verdict(false, "", "", "")) {
            GoalOutcome::GaveUp { iteration } => assert_eq!(iteration, MAX_GOAL_ITERATIONS),
            _ => panic!("expected GaveUp at the cap"),
        }
        // Stage must not be left "running", or the pipeline would wedge.
        assert!(!g.is_running());
    }

    #[test]
    fn keeps_retrying_just_below_cap() {
        let mut g = GoalState { iteration: MAX_GOAL_ITERATIONS - 1, ..Default::default() };
        assert!(matches!(g.apply_verdict(verdict(false, "", "", "")), GoalOutcome::Retry { .. }));
    }

    #[test]
    fn is_running_only_during_agent_stages() {
        let stages = [
            (GoalStage::Inactive, false),
            (GoalStage::WaitForGoal, false),
            (GoalStage::RunAgent1, true),
            (GoalStage::RunEvaluator, true),
            (GoalStage::Done, false),
        ];
        for (stage, expected) in stages {
            let g = GoalState { stage, ..Default::default() };
            assert_eq!(g.is_running(), expected);
        }
    }

    #[test]
    fn activate_starts_fresh_with_sessions() {
        let mut g = GoalState {
            iteration: 7,
            summary: "old".to_string(),
            ..Default::default()
        };
        g.activate();
        assert!(g.mode);
        assert_eq!(g.stage, GoalStage::Inactive);
        assert_eq!(g.iteration, 0);
        assert!(g.summary.is_empty());
        assert!(!g.agent1_session_id.is_empty() && !g.agent2_session_id.is_empty());
    }

    #[test]
    fn deactivate_resets_everything() {
        let mut g = GoalState {
            mode: true,
            stage: GoalStage::RunAgent1,
            iteration: 5,
            prompt: "p".to_string(),
            ..Default::default()
        };
        g.deactivate();
        assert!(!g.mode);
        assert!(!g.is_running());
        assert_eq!(g.iteration, 0);
        assert!(g.prompt.is_empty());
    }
}

impl App {
    /// Abort any in-flight goal cycle and leave goal mode fully reset, so the
    /// user is never stuck behind a "cycle in progress" that isn't running.
    /// No-op when goal mode is off.
    pub(crate) fn cancel_goal_cycle(&mut self, reason: &str) {
        if !self.state.goal.mode {
            return;
        }
        self.state.focused_mut().messages.push(ChatMessage::system(reason));
        self.state.goal.deactivate();
    }

    pub(crate) async fn run_goal_evaluation(&mut self, agent_result: String) {
        let provider = match &self.state.focused().provider {
            Some(p) => Arc::clone(p),
            None => {
                self.cancel_goal_cycle(
                    "No provider configured for evaluation — goal cancelled. Set your token, then /goal to retry.",
                );
                return;
            }
        };

        let system_prompt = self.prompts
            .goal_evaluator_prompt
            .clone();

        let user_msg = format!(
            "## Goal\n{}\n\n## Completed Work\n{}\n\n## Evaluation",
            self.state.goal.text, agent_result
        );

        let eval_messages = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&user_msg),
        ];

        let request = crate::provider::CompletionRequest {
            messages: eval_messages.clone(),
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature.min(0.5),
            max_tokens: self.config.provider.max_tokens,
            stream: false,
        };

        match provider.complete(request).await {
            Ok(response) => {
                let verdict = parse_goal_verdict(&response.content);
                let save_eval_session = |this: &Self| {
                    let _ = crate::session::save_session_with_tag(
                        &this.state.goal.agent2_session_id,
                        &this.state.focused().session_started_at,
                        &eval_messages,
                        &this.config,
                        &this.state.workspace_path,
                        Some("__goal_system__".to_string()),
                    );
                };

                match self.state.goal.apply_verdict(verdict) {
                    GoalOutcome::Succeeded { summary } => {
                        self.state.focused_mut().messages.push(ChatMessage::system(
                            &format!("✅ Goal achieved!\n\n{}", summary)
                        ));
                        save_eval_session(self);
                    }
                    GoalOutcome::GaveUp { iteration } => {
                        save_eval_session(self);
                        self.cancel_goal_cycle(&format!(
                            "🛑 Goal not achieved after {iteration} attempts — stopping. Use /goal to try a different approach."
                        ));
                    }
                    GoalOutcome::Retry { iteration, issues, feedback, swapped_agent1, swapped_agent2 } => {
                        if swapped_agent2 {
                            self.state.focused_mut().messages.push(ChatMessage::system(
                                "🔄 Swapping evaluator (agent 2) to new session after 5 failures."
                            ));
                        }
                        // Save under the (possibly swapped) evaluator session id.
                        save_eval_session(self);

                        self.state.focused_mut().messages.push(ChatMessage::system(
                            &format!(
                                "❌ Goal not achieved (attempt {}).\nIssues: {}\nFeedback: {}",
                                iteration, issues, feedback
                            )
                        ));
                        if swapped_agent1 {
                            self.state.focused_mut().messages.push(ChatMessage::system(
                                "🔄 Swapping agent to new session after 3 failures."
                            ));
                        }
                        self.retry_agent1_with_feedback(&feedback).await;
                    }
                }
            }
            Err(e) => {
                self.state.focused_mut().messages.push(ChatMessage::system(
                    &format!("⚠ Evaluation failed: {e}. Retrying agent 1...")
                ));
                self.state.goal.stage = GoalStage::RunAgent1;
                self.retry_agent1().await;
            }
        }
    }

    pub(crate) async fn handle_goal_verdict(&mut self) {
        // Find the evaluator's result from the last assistant message
        let eval_result = self.state.focused_mut().messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        if eval_result.is_empty() {
            self.state.focused_mut().messages.push(ChatMessage::system(
                "⚠ Evaluator produced no output. Retrying evaluation..."
            ));
            return;
        }

        // Re-run evaluation via the event-based path
        let verdict = parse_goal_verdict(&eval_result);
        let event = AppEvent::GoalEvaluationDone(verdict);
        let _ = self.event_tx.send(event);
    }

    pub(crate) async fn handle_goal_verdict_from(&mut self, verdict: GoalVerdict) {
        match self.state.goal.apply_verdict(verdict) {
            GoalOutcome::Succeeded { summary } => {
                self.state.focused_mut().messages.push(ChatMessage::system(
                    &format!("✅ Goal achieved!\n\n{}", summary)
                ));
            }
            GoalOutcome::GaveUp { iteration } => {
                self.cancel_goal_cycle(&format!(
                    "🛑 Goal not achieved after {iteration} attempts — stopping. Use /goal to try a different approach."
                ));
            }
            GoalOutcome::Retry { iteration, issues, feedback, swapped_agent1, swapped_agent2 } => {
                if swapped_agent2 {
                    self.state.focused_mut().messages.push(ChatMessage::system(
                        "🔄 Swapping evaluator (agent 2) to new session after 5 failures."
                    ));
                }
                self.state.focused_mut().messages.push(ChatMessage::system(
                    &format!(
                        "❌ Goal not achieved (attempt {}).\nIssues: {}\nFeedback: {}",
                        iteration, issues, feedback
                    )
                ));
                if swapped_agent1 {
                    self.state.focused_mut().messages.push(ChatMessage::system(
                        "🔄 Swapping agent to new session after 3 failures."
                    ));
                }
                self.retry_agent1_with_feedback(&feedback).await;
            }
        }
    }

    pub(crate) async fn retry_agent1(&mut self) {
        let prompt = format!(
            "Continue working on the original request.\n\nOriginal prompt: {}\n\nGoal: {}",
            self.state.goal.prompt, self.state.goal.text
        );
        let iteration = self.state.goal.iteration;
        self.state.push_message(ChatMessage::user(&format!(
            "[Continuing goal cycle - attempt {iteration}]"
        )));
        self.send_to_agent(prompt).await.unwrap_or_else(|e| {
            self.state.push_system(&format!("⚠ Failed to retry agent: {e}"));
        });
    }

    pub(crate) async fn retry_agent1_with_feedback(&mut self, feedback: &str) {
        let prompt = format!(
            "The reviewer found issues with the previous attempt. Please fix them.\n\n\
            Original request: {}\n\nGoal: {}\n\n\
            Issues to fix:\n{}\n\n\
            Previous conversation history is above. Focus on fixing the issues.",
            self.state.goal.prompt, self.state.goal.text, feedback
        );
        self.send_to_agent(prompt).await.unwrap_or_else(|e| {
            self.state.focused_mut().messages.push(ChatMessage::system(
                &format!("⚠ Failed to retry agent: {e}")
            ));
        });
    }
}
