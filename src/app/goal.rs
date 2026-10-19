//! Goal-mode logic.
//!
//! Goal mode runs a two-agent loop (a worker and an evaluator) until the
//! evaluator reports success. The state lives in [`AppState`](super::AppState)
//! today, but the *pure* pieces — parsing the evaluator's verdict out of its
//! free-text reply — belong here where they can be tested in isolation.

use super::App;
use super::events::{AppEvent, GoalEvalOutcome, GoalStage, GoalVerdict};
use crate::provider::ChatMessage;
use std::sync::Arc;

/// Stop the worker/evaluator loop after this many attempts so a goal that can
/// never be satisfied (or a flaky evaluator) doesn't retry forever.
pub(crate) const MAX_GOAL_ITERATIONS: u32 = 10;

/// All state for goal mode, grouped out of the `AppState` god-object.
///
/// Goal mode drives a two-agent loop: `agent1` (the worker) attempts the goal,
/// then an evaluator judges it; on failure the worker retries with feedback.
/// Оценщик получает свежую сессию после пяти провалов подряд — поэтому
/// его счётчик и id живут здесь.
#[derive(Debug, Clone, Default)]
pub struct GoalState {
    pub mode: bool,
    pub stage: GoalStage,
    pub prompt: String,
    pub text: String,
    pub iteration: u32,
    pub agent2_failures: u32,
    pub agent2_session_id: String,
    pub summary: String,
    /// Чат цикла. Всё — ход работника, оценка, повтор — идёт в него, а не в
    /// тот, что сейчас на экране: иначе смена чата клинила или уводила цикл.
    pub conversation: Option<super::conversation::ConversationId>,
    /// Сбоев оценки подряд: без счётчика сбой повторял работника бесконечно.
    pub eval_failures: u32,
}

/// После скольких сбоев оценки подряд цикл останавливается.
pub(crate) const MAX_EVAL_FAILURES: u32 = 3;

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
        /// A fresh agent-2 (evaluator) session was started this round.
        swapped_agent2: bool,
    },
}

impl GoalState {
    /// Применить вердикт: счётчик и смена сессии оценщика (после пяти
    /// провалов), номер попытки, следующая стадия. Возвращает, что сделать.
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

        self.stage = GoalStage::RunAgent1;

        GoalOutcome::Retry {
            iteration,
            issues: verdict.issues,
            feedback,
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
    let mut has_status = false;
    let mut summary = String::new();
    let mut issues = String::new();
    let mut feedback = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some((_, status)) = trimmed.split_once("**Status:**") {
            // Слово целиком: «UNSUCCESSFUL» и «NOT SUCCESS» раньше были успехом.
            has_status = true;
            success = first_word(status) == "SUCCESS";
        } else if trimmed.contains("**Summary:**") {
            summary = trimmed
                .split_once("**Summary:**")
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
                .to_string();
        } else if trimmed.contains("**Issues:**") {
            issues = trimmed
                .split_once("**Issues:**")
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
                .to_string();
        } else if trimmed.contains("**Feedback:**") {
            feedback = trimmed
                .split_once("**Feedback:**")
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
                .to_string();
        }
    }

    // Без строки статуса — успех, только если SUCCESS сказано словом и нигде
    // нет отрицания: «criteria for SUCCESS not met» успехом не считается.
    if !has_status {
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .map(str::to_uppercase)
            .collect();
        let negated = words.iter().any(|word| {
            matches!(
                word.as_str(),
                "NOT" | "FAIL" | "FAILED" | "FAILURE" | "UNSUCCESSFUL"
            )
        });
        success = !negated && words.iter().any(|word| word == "SUCCESS");
    }

    GoalVerdict {
        success,
        summary,
        issues,
        feedback,
    }
}

/// Первое слово значения без звёздочек и знаков: `**SUCCESS**.` → `SUCCESS`.
fn first_word(value: &str) -> String {
    value
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Подстрока SUCCESS внутри UNSUCCESSFUL раньше давала «цель достигнута».
    #[test]
    fn a_negated_status_is_not_success() {
        assert!(!parse_goal_verdict("**Status:** UNSUCCESSFUL").success);
        assert!(!parse_goal_verdict("**Status:** NOT A SUCCESS").success);
        assert!(parse_goal_verdict("**Status:** **SUCCESS**.").success);
        assert!(!parse_goal_verdict("The criteria for SUCCESS were not met.").success);
    }

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
        let v =
            parse_goal_verdict("**Status:** FAILURE\n**Summary:** partial — SUCCESS not reached");
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
        assert_eq!(g.agent2_failures, 1);
        match outcome {
            GoalOutcome::Retry {
                iteration,
                issues,
                feedback,
                swapped_agent2,
            } => {
                assert_eq!(iteration, 0); // pre-increment attempt number
                assert_eq!(issues, "tests red");
                assert_eq!(feedback, "fix tests");
                assert!(!swapped_agent2);
            }
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn apply_failure_empty_feedback_defaults() {
        let mut g = GoalState::default();
        let outcome = g.apply_verdict(verdict(false, "", "", ""));
        match outcome {
            GoalOutcome::Retry { feedback, .. } => {
                assert!(feedback.starts_with("No specific feedback"))
            }
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn evaluator_swaps_after_fifth_failure() {
        let mut g = GoalState {
            agent2_failures: 4,
            ..Default::default()
        };
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
    fn gives_up_at_iteration_cap() {
        let mut g = GoalState {
            iteration: MAX_GOAL_ITERATIONS,
            ..Default::default()
        };
        match g.apply_verdict(verdict(false, "", "", "")) {
            GoalOutcome::GaveUp { iteration } => assert_eq!(iteration, MAX_GOAL_ITERATIONS),
            _ => panic!("expected GaveUp at the cap"),
        }
        // Stage must not be left "running", or the pipeline would wedge.
        assert!(!g.is_running());
    }

    #[test]
    fn keeps_retrying_just_below_cap() {
        let mut g = GoalState {
            iteration: MAX_GOAL_ITERATIONS - 1,
            ..Default::default()
        };
        assert!(matches!(
            g.apply_verdict(verdict(false, "", "", "")),
            GoalOutcome::Retry { .. }
        ));
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
            let g = GoalState {
                stage,
                ..Default::default()
            };
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
        assert!(!g.agent2_session_id.is_empty());
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
    /// Чат цикла; до старта цикла — чат на экране.
    fn goal_conversation(&self) -> super::conversation::ConversationId {
        self.state
            .goal
            .conversation
            .unwrap_or_else(|| self.state.conversations.focused_id())
    }

    /// Заметка цикла — в его чат, а не в тот, что сейчас на экране.
    fn goal_note(&mut self, note: ChatMessage) {
        let target = self.goal_conversation();
        match self.state.conversations.get_mut(target) {
            Some(conversation) => conversation.messages.push(note),
            None => self.state.push_message(note),
        }
    }

    /// Abort any in-flight goal cycle and leave goal mode fully reset, so the
    /// user is never stuck behind a "cycle in progress" that isn't running.
    /// No-op when goal mode is off.
    pub(crate) fn cancel_goal_cycle(&mut self, reason: &str) {
        if !self.state.goal.mode {
            return;
        }
        self.goal_note(ChatMessage::system(reason));
        self.state.goal.deactivate();
    }

    /// Launch the goal evaluator as a detached task; the result comes back as
    /// `GoalEvaluationDone`. На форке провайдера: на DeepSeek оценщик иначе
    /// подменял серверную сессию работника, и тот терял всю историю.
    pub(crate) fn spawn_goal_evaluation(&mut self, agent_result: String, tool_report: String) {
        let target = self.goal_conversation();
        let provider = match self
            .state
            .conversations
            .get(target)
            .and_then(|conversation| conversation.provider.as_ref())
        {
            Some(p) => p.fork(),
            None => {
                self.cancel_goal_cycle(
                    "No provider configured for evaluation — goal cancelled. Set your token, then /goal to retry.",
                );
                return;
            }
        };

        let system_prompt = self.prompts.goal_evaluator_prompt.clone();
        // Не только самоотчёт: оценщик видит, что инструменты вернули на деле.
        let user_msg = format!(
            "## Goal\n{}\n\n## Completed Work\n{}\n\n## Tool results of that turn\n{}\n\n## Evaluation",
            self.state.goal.text, agent_result, tool_report
        );
        let mut eval_messages = vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&user_msg),
        ];

        let request = crate::provider::CompletionRequest {
            messages: eval_messages.clone(),
            tools: Vec::new(),
            model: self.config.provider.model.clone(),
            temperature: self.config.provider.temperature.min(0.5),
            max_tokens: self.config.provider.max_tokens,
            stream: false,
        };

        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let discard = crate::provider::SessionDiscard::new(Arc::clone(&provider));
            let outcome = match provider.complete(request).await {
                Ok(response) => {
                    let verdict = parse_goal_verdict(&response.content);
                    // Persist the evaluator's answer with its transcript.
                    eval_messages.push(ChatMessage::assistant(&response.content));
                    GoalEvalOutcome::Verdict {
                        verdict,
                        eval_messages,
                    }
                }
                Err(e) => GoalEvalOutcome::Failed(e.to_string()),
            };
            discard.now().await;
            let _ = event_tx.send(AppEvent::GoalEvaluationDone(outcome));
        });
    }

    /// Apply the evaluator's outcome delivered by `GoalEvaluationDone` — the
    /// single verdict path (an inline duplicate used to drift from this one).
    pub(crate) async fn handle_goal_evaluation_done(&mut self, outcome: GoalEvalOutcome) {
        // The cycle may have been cancelled (Esc, /goal off, agent error)
        // while the evaluator task was in flight — drop stale verdicts.
        if !self.state.goal.mode || self.state.goal.stage != GoalStage::RunEvaluator {
            return;
        }

        let (verdict, eval_messages) = match outcome {
            GoalEvalOutcome::Verdict {
                verdict,
                eval_messages,
            } => (verdict, eval_messages),
            GoalEvalOutcome::Failed(e) => {
                self.state.goal.eval_failures += 1;
                if self.state.goal.eval_failures >= MAX_EVAL_FAILURES {
                    self.cancel_goal_cycle(&format!(
                        "🛑 Goal evaluation failed {MAX_EVAL_FAILURES} times in a row ({e}) — stopping. Use /goal to retry."
                    ));
                    return;
                }
                self.goal_note(ChatMessage::ui_system(&format!(
                    "⚠ Evaluation failed: {e}. Retrying agent 1..."
                )));
                self.state.goal.stage = GoalStage::RunAgent1;
                self.retry_agent1().await;
                return;
            }
        };
        self.state.goal.eval_failures = 0;

        let save_eval_session = |this: &Self| {
            let started_at = this
                .state
                .conversations
                .get(this.goal_conversation())
                .map(|conversation| conversation.session_started_at.clone())
                .unwrap_or_default();
            if let Err(error) = crate::session::save_session(
                &this.state.goal.agent2_session_id,
                &started_at,
                &eval_messages,
                &this.config,
                &this.state.workspace_path,
                &crate::session::SessionMeta {
                    tag: Some("__goal_system__".to_string()),
                    ..Default::default()
                },
            ) {
                tracing::warn!("goal: could not save the evaluator transcript: {error}");
            }
        };

        match self.state.goal.apply_verdict(verdict) {
            GoalOutcome::Succeeded { summary } => {
                self.goal_note(ChatMessage::ui_system(&format!(
                    "✅ Goal achieved!\n\n{summary}"
                )));
                save_eval_session(self);
            }
            GoalOutcome::GaveUp { iteration } => {
                save_eval_session(self);
                self.cancel_goal_cycle(&format!(
                    "🛑 Goal not achieved after {iteration} attempts — stopping. Use /goal to try a different approach."
                ));
            }
            GoalOutcome::Retry {
                iteration,
                issues,
                feedback,
                swapped_agent2,
            } => {
                if swapped_agent2 {
                    self.goal_note(ChatMessage::ui_system(
                        "🔄 Swapping evaluator (agent 2) to new session after 5 failures.",
                    ));
                }
                // Save under the (possibly swapped) evaluator session id.
                save_eval_session(self);

                self.goal_note(ChatMessage::ui_system(&format!(
                    "❌ Goal not achieved (attempt {iteration}).\nIssues: {issues}\nFeedback: {feedback}"
                )));
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
        // The full prompt is what the model receives; the chat shows a short
        // label instead of repeating the goal text every iteration.
        let message = ChatMessage::user_with_display(
            &prompt,
            &format!("[Continuing goal cycle — attempt {iteration}]"),
        );
        self.send_goal_turn(message).await;
    }

    pub(crate) async fn retry_agent1_with_feedback(&mut self, feedback: &str) {
        let prompt = format!(
            "The reviewer found issues with the previous attempt. Please fix them.\n\n\
            Original request: {}\n\nGoal: {}\n\n\
            Issues to fix:\n{}\n\n\
            Previous conversation history is above. Focus on fixing the issues.",
            self.state.goal.prompt, self.state.goal.text, feedback
        );
        let iteration = self.state.goal.iteration;
        let message = ChatMessage::user_with_display(
            &prompt,
            &format!("[Goal retry — attempt {iteration}: applying evaluator feedback]"),
        );
        self.send_goal_turn(message).await;
    }

    /// Ход работника — в чат цикла, где бы ни был человек.
    async fn send_goal_turn(&mut self, message: ChatMessage) {
        let target = self.goal_conversation();
        if let Err(e) = self.send_turn(target, Some(message)).await {
            self.goal_note(ChatMessage::system(&format!(
                "⚠ Failed to retry agent: {e}"
            )));
        }
    }
}

impl App {
    /// GOAL-cycle transition run after the goal chat's turn ends with `Done`:
    /// hand the worker's output to the evaluator — or retry an empty turn.
    pub(crate) async fn maybe_advance_goal_cycle(&mut self) {
        if !(self.state.goal.mode && self.state.goal.stage == GoalStage::RunAgent1) {
            return;
        }
        let target = self.goal_conversation();
        let Some(conversation) = self.state.conversations.get(target) else {
            self.cancel_goal_cycle("The goal's chat is gone — goal cancelled.");
            return;
        };
        let agent_result = conversation
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::provider::Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let tool_report = last_turn_tool_report(&conversation.messages);

        if !agent_result.is_empty() {
            self.state.status_message = "Evaluating goal...".to_string();
            self.state.goal.stage = GoalStage::RunEvaluator;
            self.goal_note(ChatMessage::ui_system(
                "🔍 Evaluating result against goal...",
            ));
            self.spawn_goal_evaluation(agent_result, tool_report);
        } else {
            self.goal_note(ChatMessage::ui_system(
                "⚠ Agent produced no output. Retrying...",
            ));
            self.retry_agent1().await;
        }
    }
}

/// Сколько вывода одного инструмента показать оценщику.
const TOOL_REPORT_BYTES: usize = 300;

/// Результаты инструментов последнего хода — с пометкой провала. Иначе
/// оценщик судил по самоотчёту, и «тесты прошли» над упавшим `cargo test` проходило.
fn last_turn_tool_report(messages: &[ChatMessage]) -> String {
    let start = messages
        .iter()
        .rposition(|m| m.role == crate::provider::Role::User && !m.ui_only)
        .map_or(0, |index| index + 1);
    let lines: Vec<String> = messages[start..]
        .iter()
        .filter(|m| m.role == crate::provider::Role::Tool)
        .map(|m| {
            format!(
                "- {}{}: {}",
                m.name.as_deref().unwrap_or("tool"),
                if m.tool_error { " (FAILED)" } else { "" },
                crate::util::truncate_with_ellipsis(m.content.trim(), TOOL_REPORT_BYTES)
            )
        })
        .collect();
    if lines.is_empty() {
        "(no tools were run)".to_string()
    } else {
        lines.join("\n")
    }
}

impl App {
    /// Empty submission while defining a goal: nudge instead of silently
    /// ignoring it (part of `submit_input`'s goal interception — kept here
    /// with the rest of the goal state machine).
    pub(crate) fn maybe_nudge_empty_goal(&mut self) {
        if !(self.state.goal.mode
            && matches!(
                self.state.goal.stage,
                GoalStage::Inactive | GoalStage::WaitForGoal
            ))
        {
            return;
        }
        let what = if matches!(self.state.goal.stage, GoalStage::Inactive) {
            "prompt"
        } else {
            "goal"
        };
        self.state.push_system(&format!(
            "Your {what} is empty — type something before pressing Enter."
        ));
        self.state.input.clear_buffer();
    }

    /// Goal-mode interception of a non-command submission: the first input
    /// becomes the prompt, the second the goal (which launches agent 1), and
    /// input during a running cycle is blocked. Returns `true` when the
    /// input was consumed by the state machine; `false` (the `Done` stage)
    /// means goal mode just ended and the caller should treat the input as a
    /// normal turn.
    pub(crate) async fn handle_goal_input(&mut self, input: &str) -> crate::error::AppResult<bool> {
        match self.state.goal.stage {
            GoalStage::Inactive => {
                // First input in goal mode = the prompt. Echoes are ui_only:
                // the model gets the combined agent-1 prompt once, later.
                self.state.goal.prompt = input.to_string();
                self.state.goal.stage = GoalStage::WaitForGoal;
                let mut echo = ChatMessage::user(input);
                echo.ui_only = true;
                self.state.focused_mut().messages.push(echo);
                self.state
                    .focused_mut()
                    .messages
                    .push(ChatMessage::ui_system(
                        "🎯 Goal mode: now define your GOAL (what must be achieved)",
                    ));
                Ok(true)
            }
            GoalStage::WaitForGoal => {
                // Second input = the goal. Цикл привязывается к этому чату.
                self.state.goal.text = input.to_string();
                self.state.goal.conversation = Some(self.state.conversations.focused_id());
                let mut echo = ChatMessage::user(&format!("GOAL: {}", input));
                echo.ui_only = true;
                self.state.focused_mut().messages.push(echo);

                // Without a provider the worker can't run; advancing the
                // stage would wedge the cycle, so bail cleanly.
                if self.state.focused().provider.is_none() {
                    self.cancel_goal_cycle(
                        "No provider configured — cannot run the goal. Set your DeepSeek token, then /goal to retry.",
                    );
                    return Ok(true);
                }

                self.state.goal.stage = GoalStage::RunAgent1;
                self.state.goal.iteration = 1;

                // Build the agent 1 prompt: user's prompt + goal.
                let agent1_prompt = format!(
                    "{}\n\nIMPORTANT - GOAL to achieve: {}",
                    self.state.goal.prompt, self.state.goal.text
                );
                let message = ChatMessage::user_with_display(
                    &agent1_prompt,
                    "[Goal cycle started — attempt 1]",
                );
                self.send_focused_turn(Some(message)).await?;
                Ok(true)
            }
            // Цикл идёт в другом чате — здесь обычный ввод.
            GoalStage::RunAgent1 | GoalStage::RunEvaluator
                if self.state.goal.conversation != Some(self.state.conversations.focused_id()) =>
            {
                Ok(false)
            }
            GoalStage::RunAgent1 | GoalStage::RunEvaluator => {
                // Block input while the goal cycle is active.
                self.state
                    .focused_mut()
                    .messages
                    .push(ChatMessage::ui_system(
                        "Goal cycle in progress. Wait for it to finish or type /goal to cancel.",
                    ));
                Ok(true)
            }
            GoalStage::Done => {
                // After the goal is done, regular input resumes.
                self.state.goal.mode = false;
                self.state.goal.stage = GoalStage::Inactive;
                Ok(false)
            }
        }
    }
}
