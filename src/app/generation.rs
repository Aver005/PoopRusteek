//! Generation status and last-turn stats.
//!
//! While the agent is streaming a reply the TUI shows a spinner, elapsed time
//! and, once finished, the token count / duration / model of the last turn.
//! These eight fields were scattered across the `AppState` god-object (several
//! of them read only by the renderer); grouping them here keeps the
//! generation lifecycle in one place.

use std::time::Instant;

/// Live generation status plus stats from the most recent completed turn.
#[derive(Default)]
pub struct GenerationState {
    /// Whether the agent is currently streaming a reply.
    pub active: bool,
    /// When the current generation started, for the elapsed-time display.
    pub start_time: Option<Instant>,
    /// Spinner animation counter, advanced on each tick while `active`.
    pub animation_tick: u64,
    /// Token count of the last completed turn.
    pub last_tokens: u32,
    /// Wall-clock duration of the last completed turn, in seconds.
    pub last_duration_secs: f64,
    /// Model that produced the last turn.
    pub last_model: String,
    /// Status string of the last assistant message (e.g. finish reason).
    pub last_status: Option<String>,
    /// "Thinking" fragment count of the last turn.
    pub last_think_fragments: u32,
}

impl GenerationState {
    /// Mark a generation as started and begin the elapsed timer. The caller
    /// passes `now` (rather than reading the clock here) so the timer logic is
    /// testable.
    pub fn begin(&mut self, now: Instant) {
        self.active = true;
        self.start_time = Some(now);
    }

    /// Take the elapsed seconds since [`begin`](Self::begin), clearing the
    /// timer. Returns `None` if no generation was in progress.
    pub fn take_elapsed(&mut self) -> Option<f64> {
        self.start_time.take().map(|start| start.elapsed().as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_marks_active_and_starts_timer() {
        let mut g = GenerationState::default();
        g.begin(Instant::now());
        assert!(g.active);
        assert!(g.start_time.is_some());
    }

    #[test]
    fn take_elapsed_returns_and_clears() {
        let mut g = GenerationState::default();
        assert_eq!(g.take_elapsed(), None); // nothing in progress

        g.begin(Instant::now());
        let elapsed = g.take_elapsed();
        assert!(elapsed.is_some_and(|e| e >= 0.0));
        assert!(g.start_time.is_none()); // timer cleared

        assert_eq!(g.take_elapsed(), None); // idempotent once taken
    }
}
