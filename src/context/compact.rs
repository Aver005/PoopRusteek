//! Rung 3's executor: takes a plan from `modes`, runs it through the model
//! using the form in `summary`, and hands back a rewritten history.
//!
//! Knows nothing about `App` — it takes a provider handle and messages, and
//! returns messages (invariant 6). The caller decides what to do with them.

use crate::context::modes::{self, CompactMode};
use crate::context::summary;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider};
use std::sync::Arc;

/// Ceiling for the summariser's own reply. Not a target: a reasoning model
/// left unbounded spends the whole budget thinking and returns no text — the
/// failure Cline documents at exactly this constant.
const SUMMARY_MAX_TOKENS: u32 = 4_096;

/// What one compaction did, for the status line and the trace.
#[derive(Debug)]
pub struct CompactOutcome {
    pub messages: Vec<ChatMessage>,
    pub calls: usize,
    pub before_tokens: u32,
    pub after_tokens: u32,
}

/// Marks the message that carries a summary, so the next compaction can feed it
/// back as the prior summary instead of summarising a summary.
pub const SUMMARY_PREFIX: &str = "[Context summary]\n";

/// The newest summary already in this history, if any.
fn prior_summary(messages: &[ChatMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.content.starts_with(SUMMARY_PREFIX))
        .map(|message| message.content.as_str())
}

/// Run `mode` over `messages`. Returns `Err` with a human-readable reason when
/// nothing was done — the caller reports it and leaves the history alone.
pub async fn compact(
    provider: &Arc<dyn LLMProvider>,
    messages: &[ChatMessage],
    mode: CompactMode,
    usable: u32,
    model: &str,
) -> Result<CompactOutcome, String> {
    let plan = modes::plan(messages, mode, usable);
    if plan.is_empty() {
        return Err("Nothing to compact yet — there is only one turn.".to_string());
    }

    let prior = prior_summary(messages).map(str::to_string);
    let mut summaries = Vec::new();
    let mut calls = 0;
    for (index, range) in plan.summarize.iter().enumerate() {
        if range.is_empty() {
            continue;
        }
        // Only the first chunk merges the previous summary: the later chunks
        // are newer material, and re-feeding it to each would duplicate it.
        let prior_for_chunk = if index == 0 { prior.as_deref() } else { None };
        let request = summary::build_request(&messages[range.clone()], prior_for_chunk);
        let response = provider
            .complete(CompletionRequest {
                messages: vec![
                    ChatMessage::system(&request.system_prompt),
                    ChatMessage::user(&request.user_prompt),
                ],
                model: model.to_string(),
                temperature: 0.2,
                max_tokens: SUMMARY_MAX_TOKENS,
                stream: false,
            })
            .await
            .map_err(|e| format!("Summariser call failed: {e}"))?;
        calls += 1;
        let text = summary::validate(&response.content).map_err(|e| e.to_string())?;
        summaries.push(text);
    }

    if summaries.is_empty() {
        return Err("Nothing to compact yet — there is only one turn.".to_string());
    }

    let mut rebuilt = Vec::new();
    if let Some(head) = plan.head.clone() {
        rebuilt.extend_from_slice(&messages[head]);
    }
    let body = format!(
        "{SUMMARY_PREFIX}{}{}",
        summaries.join("\n\n"),
        summary::observed_facts(&messages[..plan.tail.start])
    );
    // The summary arrives as a user message: every studied implementation does
    // this, because an assistant-role summary reads as something the model
    // said and invites it to continue from there.
    rebuilt.push(ChatMessage::user(&body));
    rebuilt.extend_from_slice(&messages[plan.tail.clone()]);

    let before = crate::context::conversation_tokens("", messages);
    let after = crate::context::conversation_tokens("", &rebuilt);
    // A summary longer than what it replaced is a loss twice over. Checked
    // after the fact only because the cost is already paid; the caller keeps
    // its original history.
    if after >= before {
        return Err(format!(
            "Summary came out no smaller than the history ({after} vs {before} tokens) — keeping the original."
        ));
    }
    Ok(CompactOutcome {
        messages: rebuilt,
        calls,
        before_tokens: before,
        after_tokens: after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    const FORM: &str = "## Objective\ngoal\n## Important Details\n(none)\n## Work State\n### Completed\ndone\n### Active\n(none)\n### Blocked\n(none)\n## Next Move\n1. carry on\n## Relevant Files\n- src/x.rs: touched";

    fn history(turns: usize) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        for turn in 0..turns {
            messages.push(ChatMessage::user(&format!("ask {turn}")));
            messages.push(ChatMessage::assistant(&"filler ".repeat(400)));
        }
        messages
    }

    #[tokio::test]
    async fn a_summary_replaces_the_middle_and_keeps_both_ends() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response(FORM));
        let messages = history(5);
        let outcome = compact(
            &provider,
            &messages,
            CompactMode::FirstAndLast,
            40_000,
            "fake",
        )
        .await
        .expect("compaction should succeed");

        assert_eq!(outcome.calls, 1);
        assert!(outcome.after_tokens < outcome.before_tokens);
        // Opening turn verbatim, then the summary, then the closing turn.
        assert_eq!(outcome.messages[0].content, "ask 0");
        assert!(outcome.messages[2].content.starts_with(SUMMARY_PREFIX));
        assert_eq!(outcome.messages[3].content, "ask 4");
    }

    #[tokio::test]
    async fn mode_two_calls_the_model_once_per_chunk() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_responses(
            std::iter::repeat_n(FORM.to_string(), 8).collect(),
        ));
        let messages = history(12);
        let outcome = compact(
            &provider,
            &messages,
            CompactMode::ChunkedHead,
            40_000,
            "fake",
        )
        .await
        .expect("compaction should succeed");
        assert!(
            outcome.calls > 1,
            "mode 2 should summarise chunk by chunk, made {} call(s)",
            outcome.calls
        );
    }

    #[tokio::test]
    async fn a_truncated_summary_leaves_the_history_alone() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response(
            // Output tokens ran out partway through section three.
            "## Objective\ngoal\n## Important Details\n(none)\n## Work St",
        ));
        let messages = history(4);
        let error = compact(
            &provider,
            &messages,
            CompactMode::AllButLast,
            40_000,
            "fake",
        )
        .await
        .expect_err("a half-written form must be refused");
        assert!(error.contains("Work State"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn one_turn_is_not_worth_a_model_call() {
        let provider: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response(FORM));
        let messages = history(1);
        let error = compact(
            &provider,
            &messages,
            CompactMode::AllButLast,
            40_000,
            "fake",
        )
        .await
        .expect_err("one turn cannot be compacted");
        assert!(error.contains("one turn"), "unexpected error: {error}");
    }
}
