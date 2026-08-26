//! Where each compaction mode cuts the history. Pure index maths — no model
//! call, no I/O — so every boundary rule is unit-testable on its own.
//!
//! A *turn* here starts at a user message and runs up to the message before
//! the next one. Tool results and assistant replies belong to the turn that
//! provoked them, so a cut never separates a call from its result.

use crate::context::budget_tokens;
use crate::provider::{ChatMessage, Role};
use std::ops::Range;

/// Chunk size for mode 2, in budget tokens. Chosen over a fixed chunk *count*:
/// ten chunks of a 3 000-token history are 300 tokens each, too small to be
/// worth a model call, while ten chunks of a 200 000-token one are too big.
const MODE2_CHUNK_TOKENS: u32 = 2_000;

/// Mode 1 keeps the opening turn only while it fits this share of the usable
/// window. A pasted 50 KB log as turn one must not survive verbatim — it would
/// eat the room the compaction is trying to free.
const MODE1_HEAD_SHARE: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactMode {
    /// First turn and last turn verbatim, everything between them summarised.
    FirstAndLast,
    /// The oldest half split into chunks, each summarised on its own.
    ChunkedHead,
    /// Everything before the last turn summarised into one block.
    AllButLast,
}

impl CompactMode {
    /// Out-of-range numbers fall back to mode 1 rather than failing: the value
    /// reaches us from a config file that must keep loading.
    pub fn from_number(n: u8) -> Self {
        match n {
            2 => Self::ChunkedHead,
            3 => Self::AllButLast,
            _ => Self::FirstAndLast,
        }
    }

    pub fn number(self) -> u8 {
        match self {
            Self::FirstAndLast => 1,
            Self::ChunkedHead => 2,
            Self::AllButLast => 3,
        }
    }
}

/// What to send to the summariser and what to keep untouched. Ranges index
/// into the message slice the plan was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactPlan {
    /// Kept verbatim ahead of the summary (mode 1's opening turn).
    pub head: Option<Range<usize>>,
    /// One entry per summariser call, in order. Empty means nothing to do.
    pub summarize: Vec<Range<usize>>,
    /// Kept verbatim after the summary — always at least the last turn.
    pub tail: Range<usize>,
}

impl CompactPlan {
    /// Nothing worth compacting: the caller should say so rather than spend a
    /// model call to summarise two messages.
    pub fn is_empty(&self) -> bool {
        self.summarize.iter().all(|range| range.is_empty())
    }
}

/// Index where each turn starts. A turn begins at a user message that is not a
/// tool result; everything after it belongs to that turn.
fn turn_starts(messages: &[ChatMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User && !message.ui_only)
        .map(|(index, _)| index)
        .collect()
}

/// One range as a plan. Written out rather than `vec![a..b]`, which clippy
/// flags as easy to misread for a list of values.
fn single(range: Range<usize>) -> Vec<Range<usize>> {
    vec![range]
}

fn range_tokens(messages: &[ChatMessage], range: &Range<usize>) -> u32 {
    messages[range.clone()]
        .iter()
        .filter(|message| !message.ui_only)
        .map(|message| budget_tokens(&message.content))
        .sum()
}

/// Build the plan for `mode`. `usable` is the budget the window leaves for the
/// conversation; it only matters to mode 1, which uses it to decide whether the
/// opening turn is small enough to keep.
pub fn plan(messages: &[ChatMessage], mode: CompactMode, usable: u32) -> CompactPlan {
    let starts = turn_starts(messages);
    // Fewer than two turns: there is no "before the last turn" to summarise.
    if starts.len() < 2 {
        return CompactPlan {
            head: None,
            summarize: Vec::new(),
            tail: 0..messages.len(),
        };
    }
    let last = starts[starts.len() - 1];

    match mode {
        CompactMode::AllButLast => CompactPlan {
            head: None,
            summarize: single(0..last),
            tail: last..messages.len(),
        },
        CompactMode::FirstAndLast => {
            // From 0, not from the first user message: whatever precedes it is
            // history too, and modes 2 and 3 summarise it rather than drop it.
            let opening = 0..starts[1];
            let budget = usable / MODE1_HEAD_SHARE;
            // Over budget, the opening turn is summarised with the middle
            // instead of being kept — see MODE1_HEAD_SHARE.
            if range_tokens(messages, &opening) > budget {
                return CompactPlan {
                    head: None,
                    summarize: single(0..last),
                    tail: last..messages.len(),
                };
            }
            CompactPlan {
                head: Some(opening.clone()),
                summarize: single(opening.end..last),
                tail: last..messages.len(),
            }
        }
        CompactMode::ChunkedHead => {
            // Split at a turn boundary nearest the halfway point, so a chunk
            // never starts mid-turn.
            let total = range_tokens(messages, &(0..last));
            let half = total / 2;
            let mut running = 0;
            let mut split = last;
            for pair in starts.windows(2) {
                running += range_tokens(messages, &(pair[0]..pair[1]));
                if running >= half {
                    split = pair[1];
                    break;
                }
            }
            CompactPlan {
                head: None,
                summarize: chunk_ranges(messages, 0..split, &starts),
                tail: split..messages.len(),
            }
        }
    }
}

/// Cut `span` into turn-aligned chunks of roughly [`MODE2_CHUNK_TOKENS`].
fn chunk_ranges(
    messages: &[ChatMessage],
    span: Range<usize>,
    starts: &[usize],
) -> Vec<Range<usize>> {
    if span.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut chunk_start = span.start;
    let mut running = 0;
    for pair in starts.windows(2) {
        if pair[0] < span.start || pair[1] > span.end {
            continue;
        }
        running += range_tokens(messages, &(pair[0]..pair[1]));
        if running >= MODE2_CHUNK_TOKENS {
            chunks.push(chunk_start..pair[1]);
            chunk_start = pair[1];
            running = 0;
        }
    }
    if chunk_start < span.end {
        chunks.push(chunk_start..span.end);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` turns, each a user message plus one assistant reply of `chars`.
    fn history(turns: usize, chars: usize) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        for turn in 0..turns {
            messages.push(ChatMessage::user(&format!("ask {turn}")));
            messages.push(ChatMessage::assistant(&"x".repeat(chars)));
        }
        messages
    }

    #[test]
    fn a_single_turn_is_never_compacted() {
        let messages = history(1, 100);
        for mode in [
            CompactMode::FirstAndLast,
            CompactMode::ChunkedHead,
            CompactMode::AllButLast,
        ] {
            let plan = plan(&messages, mode, 10_000);
            assert!(plan.is_empty(), "{mode:?} tried to summarise one turn");
            assert_eq!(plan.tail, 0..messages.len());
        }
    }

    #[test]
    fn mode_three_summarises_everything_before_the_last_turn() {
        let messages = history(4, 100);
        let plan = plan(&messages, CompactMode::AllButLast, 10_000);
        assert_eq!(plan.head, None);
        assert_eq!(plan.summarize, single(0..6));
        assert_eq!(plan.tail, 6..8);
    }

    #[test]
    fn mode_one_keeps_the_opening_and_closing_turns() {
        let messages = history(5, 100);
        let plan = plan(&messages, CompactMode::FirstAndLast, 10_000);
        assert_eq!(plan.head, Some(0..2));
        assert_eq!(plan.summarize, single(2..8));
        assert_eq!(plan.tail, 8..10);
    }

    #[test]
    fn mode_one_gives_up_on_an_opening_turn_that_would_eat_the_window() {
        // Turn one is a pasted log: far over a quarter of the usable window.
        let mut messages = vec![
            ChatMessage::user("here is the log"),
            ChatMessage::assistant(&"x".repeat(60_000)),
        ];
        messages.extend(history(3, 100));
        let plan = plan(&messages, CompactMode::FirstAndLast, 10_000);
        assert_eq!(plan.head, None, "an oversized opening turn is summarised");
        assert!(plan.summarize[0].start == 0);
    }

    #[test]
    fn mode_two_chunks_the_oldest_half_and_keeps_the_rest() {
        // Each turn is ~1000 budget tokens, so chunks land every ~2 turns.
        let messages = history(10, 3_000);
        let plan = plan(&messages, CompactMode::ChunkedHead, 20_000);
        assert!(
            plan.summarize.len() > 1,
            "mode 2 should produce several chunks, got {:?}",
            plan.summarize
        );
        // Chunks are contiguous, in order, and stop where the tail begins.
        assert_eq!(plan.summarize[0].start, 0);
        for pair in plan.summarize.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert_eq!(plan.summarize.last().unwrap().end, plan.tail.start);
    }

    #[test]
    fn every_chunk_starts_on_a_turn_boundary() {
        let messages = history(10, 3_000);
        let plan = plan(&messages, CompactMode::ChunkedHead, 20_000);
        for range in &plan.summarize {
            assert_eq!(
                messages[range.start].role,
                Role::User,
                "chunk {range:?} starts mid-turn"
            );
        }
    }

    #[test]
    fn no_mode_silently_drops_a_message() {
        // Anything ahead of the first user message is still history: it must be
        // kept verbatim or summarised, never simply lost.
        let mut messages = vec![ChatMessage::system("project rules")];
        messages.extend(history(4, 100));
        for mode in [
            CompactMode::FirstAndLast,
            CompactMode::ChunkedHead,
            CompactMode::AllButLast,
        ] {
            let plan = plan(&messages, mode, 10_000);
            let mut covered = vec![false; messages.len()];
            for range in plan
                .head
                .iter()
                .cloned()
                .chain(plan.summarize.iter().cloned())
                .chain(std::iter::once(plan.tail.clone()))
            {
                for index in range {
                    covered[index] = true;
                }
            }
            let lost: Vec<usize> = (0..messages.len()).filter(|i| !covered[*i]).collect();
            assert!(
                lost.is_empty(),
                "{mode:?} dropped messages {lost:?}: {plan:?}"
            );
        }
    }

    #[test]
    fn a_stale_mode_number_falls_back_to_one() {
        assert_eq!(CompactMode::from_number(0), CompactMode::FirstAndLast);
        assert_eq!(CompactMode::from_number(9), CompactMode::FirstAndLast);
        assert_eq!(CompactMode::from_number(2), CompactMode::ChunkedHead);
        assert_eq!(CompactMode::from_number(3).number(), 3);
    }
}
