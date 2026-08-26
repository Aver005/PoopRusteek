//! Rung 3's executor: takes a plan from `modes`, runs it through the model
//! using the form in `summary`, and hands back a rewritten history.
//!
//! Knows nothing about `App` — it takes a provider handle and messages, and
//! returns messages (invariant 6). The caller decides what to do with them.

use crate::context::modes::{self, CompactMode};
use crate::context::summary;
use crate::provider::{ChatMessage, CompletionRequest, LLMProvider, Role};
use std::borrow::Cow;
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

/// Where the newest summary sits in this history, if it carries one. A message
/// counts only when we could have written it ourselves: a marker-shaped line in
/// an assistant reply or a pasted user message is not a summary.
fn prior_summary(messages: &[ChatMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            let looks_ours = message.role == Role::User
                && !message.ui_only
                && message.content.starts_with(SUMMARY_PREFIX)
                && summary::validate(&message.content).is_ok();
            looks_ours.then_some(index)
        })
}

/// Run `mode` over `messages`. Returns `Err` with a human-readable reason when
/// nothing was done — the caller reports it and leaves the history alone.
///
/// `provider` must be a throwaway fork: between chunks this drops its
/// server-side session, so the handle must not be a live chat's.
pub async fn compact(
    provider: &Arc<dyn LLMProvider>,
    messages: &[ChatMessage],
    mode: CompactMode,
    usable: u32,
    model: &str,
) -> Result<CompactOutcome, String> {
    // The previous summary belongs in <prior-summary>, never in the material
    // being summarised: left in place, the model merges it with itself, and a
    // repeated `/compact` has nothing else to summarise at all.
    let (working, prior): (Cow<'_, [ChatMessage]>, Option<String>) = match prior_summary(messages) {
        Some(index) => {
            let mut rest = messages.to_vec();
            let summary = rest.remove(index);
            (Cow::Owned(rest), Some(summary.content))
        }
        None => (Cow::Borrowed(messages), None),
    };

    let plan = modes::plan(&working, mode, usable);
    if plan.is_empty() {
        return Err("Nothing to compact yet — there is only one turn.".to_string());
    }

    let mut summaries = Vec::new();
    let mut calls = 0;
    for (index, range) in plan.summarize.iter().enumerate() {
        if range.is_empty() {
            continue;
        }
        // A provider that keeps history upstream would otherwise answer this
        // chunk inside the branch the previous chunks filled.
        if calls > 0 && provider.keeps_server_side_history() {
            let _ = provider.discard_remote_session().await;
            let _ = provider.reset().await;
        }
        // Only the first chunk merges the previous summary: the later chunks
        // are newer material, and re-feeding it to each would duplicate it.
        let prior_for_chunk = if index == 0 { prior.as_deref() } else { None };
        let request = summary::build_request(&working[range.clone()], prior_for_chunk);
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
        rebuilt.extend_from_slice(&working[head]);
    }
    let body = format!(
        "{SUMMARY_PREFIX}{}{}",
        summary::merge(&summaries),
        summary::observed_facts(&working[..plan.tail.start])
    );
    // The summary arrives as a user message: every studied implementation does
    // this, because an assistant-role summary reads as something the model
    // said and invites it to continue from there.
    rebuilt.push(ChatMessage::user(&body));
    rebuilt.extend_from_slice(&working[plan.tail.clone()]);

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

    /// The form with every bullet tagged, so a merged summary can be checked
    /// for what survived from each chunk.
    fn form(tag: &str) -> String {
        format!(
            "## Objective\n- goal {tag}\n## Important Details\n- detail {tag}\n## Work State\n### Completed\n- done {tag}\n### Active\n(none)\n### Blocked\n(none)\n## Next Move\n1. step {tag}\n## Relevant Files\n- src/{tag}.rs: touched"
        )
    }

    fn chunk_forms() -> Vec<String> {
        (0..8).map(|i| form(&format!("c{i}"))).collect()
    }

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
    async fn only_a_summary_we_wrote_counts_as_the_prior_one() {
        let mut messages = history(4);
        // The model quoting the marker back at us is not a summary of ours.
        messages[1] = ChatMessage::assistant(&format!("{SUMMARY_PREFIX}{FORM}"));
        let fake = Arc::new(FakeProvider::with_response(FORM));
        let provider: Arc<dyn LLMProvider> = fake.clone();
        compact(
            &provider,
            &messages,
            CompactMode::AllButLast,
            40_000,
            "fake",
        )
        .await
        .expect("compaction should succeed");

        let prompt = &fake.request(0).expect("the summariser was called")[1].content;
        assert!(
            !prompt.contains("<prior-summary>"),
            "an assistant message was mistaken for a summary:\n{prompt}"
        );
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
    async fn compacting_again_with_no_turn_between_costs_nothing() {
        let first: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response(FORM));
        let once = compact(&first, &history(4), CompactMode::AllButLast, 40_000, "fake")
            .await
            .expect("first compaction");

        let fake = Arc::new(FakeProvider::with_response(FORM));
        let provider: Arc<dyn LLMProvider> = fake.clone();
        let again = compact(
            &provider,
            &once.messages,
            CompactMode::AllButLast,
            40_000,
            "fake",
        )
        .await;
        assert!(
            fake.request(0).is_none(),
            "the summariser was paid to summarise its own summary"
        );
        assert!(
            again.is_err(),
            "with no new turn there is nothing left to compact"
        );
    }

    #[tokio::test]
    async fn the_previous_summary_is_not_part_of_the_material_it_summarises() {
        let first: Arc<dyn LLMProvider> = Arc::new(FakeProvider::with_response(FORM));
        let mut messages = compact(&first, &history(4), CompactMode::AllButLast, 40_000, "fake")
            .await
            .expect("first compaction")
            .messages;
        // One more turn, so the second compaction has fresh material.
        messages.push(ChatMessage::user("ask again"));
        messages.push(ChatMessage::assistant(&"filler ".repeat(400)));

        let fake = Arc::new(FakeProvider::with_responses(vec![form("new")]));
        let provider: Arc<dyn LLMProvider> = fake.clone();
        let outcome = compact(
            &provider,
            &messages,
            CompactMode::AllButLast,
            40_000,
            "fake",
        )
        .await
        .expect("second compaction");

        let sent = fake.request(0).expect("the summariser was called");
        let prompt = &sent[1].content;
        let transcript = prompt.split("</conversation>").next().expect("block");
        assert!(
            !transcript.contains(SUMMARY_PREFIX),
            "the old summary is inside <conversation> as well as <prior-summary>:\n{transcript}"
        );
        assert!(
            prompt.contains("<prior-summary>"),
            "the old summary must still reach the model as the prior summary"
        );
        assert_eq!(
            outcome
                .messages
                .iter()
                .filter(|message| message.content.starts_with(SUMMARY_PREFIX))
                .count(),
            1,
            "the rewritten history kept two summaries"
        );
    }

    #[tokio::test]
    async fn many_chunks_still_produce_a_single_summary() {
        let fake = Arc::new(FakeProvider::with_responses(chunk_forms()));
        let provider: Arc<dyn LLMProvider> = fake.clone();
        let outcome = compact(
            &provider,
            &history(24),
            CompactMode::ChunkedHead,
            40_000,
            "fake",
        )
        .await
        .expect("compaction should succeed");
        assert!(
            outcome.calls > 2,
            "want several chunks, got {}",
            outcome.calls
        );

        let summary = &outcome
            .messages
            .iter()
            .find(|message| message.content.starts_with(SUMMARY_PREFIX))
            .expect("a summary message")
            .content;
        for heading in [
            "## Objective",
            "## Important Details",
            "## Work State",
            "### Completed",
            "## Next Move",
            "## Relevant Files",
        ] {
            assert_eq!(
                summary.matches(heading).count(),
                1,
                "`{heading}` appears {} times in one summary:\n{summary}",
                summary.matches(heading).count()
            );
        }
        for index in 0..outcome.calls {
            assert!(
                summary.contains(&format!("src/c{index}.rs")),
                "chunk {index} was dropped by the merge:\n{summary}"
            );
        }
    }

    #[tokio::test]
    async fn every_chunk_is_summarised_in_its_own_session() {
        let fake = Arc::new(FakeProvider::with_responses(chunk_forms()).server_side_history());
        let provider: Arc<dyn LLMProvider> = fake.clone();
        let outcome = compact(
            &provider,
            &history(24),
            CompactMode::ChunkedHead,
            40_000,
            "fake",
        )
        .await
        .expect("compaction should succeed");
        assert!(
            outcome.calls > 2,
            "want several chunks, got {}",
            outcome.calls
        );
        assert_eq!(
            fake.resets(),
            outcome.calls - 1,
            "chunk N was answered inside the session chunks 1..N-1 already filled"
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
