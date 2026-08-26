//! The compaction settings a turn carries with it.
//!
//! `run_agent_loop` is the only place the ladder runs, so every caller that
//! builds a `TurnSpec` — the TUI, a background sidechat, the headless harness —
//! hands it the same settings through [`ContextSpec`].

use crate::context::ContextBudget;
use std::path::PathBuf;

/// Rung-1 settings for one turn: the window it measures against and where the
/// cleared tool bodies are spilled. `Default` is the ladder switched off.
#[derive(Debug, Clone, Default)]
pub struct ContextSpec {
    /// Master switch for the whole ladder (`[context] auto_compact`).
    pub auto_compact: bool,
    /// Window from config; `0` means "ask the provider".
    pub context_window: u32,
    /// Window the provider reported; `0` when nobody knows.
    pub provider_window: u32,
    /// Headroom subtracted from the window for the model's own answer, after
    /// [`ContextSpec::with_output_cap`] has bounded it by `max_tokens`.
    pub reserved_tokens: u32,
    /// Verbatim tail kept, in tokens. `0` = derive it from the usable window.
    pub preserve_recent_tokens: u32,
    /// Directory the full tool outputs are written to.
    pub spill_dir: PathBuf,
}

impl ContextSpec {
    pub fn new(
        config: &crate::config::ContextConfig,
        provider_window: u32,
        session_id: &str,
    ) -> Self {
        Self {
            auto_compact: config.auto_compact,
            context_window: config.context_window,
            provider_window,
            reserved_tokens: config.reserved_tokens,
            preserve_recent_tokens: config.preserve_recent_tokens,
            spill_dir: crate::config::Config::data_dir()
                .join("tool-output")
                .join(session_id),
        }
    }

    /// §4's rule: the reserve is `min(reserved_tokens, max_tokens)` — headroom
    /// for an answer can never exceed what the model is allowed to emit.
    pub fn with_output_cap(mut self, max_tokens: u32) -> Self {
        self.reserved_tokens = bound_reserve(self.reserved_tokens, max_tokens);
        self
    }

    /// Config wins over the provider — `learn_provider_window` enforces that.
    pub fn budget(&self) -> ContextBudget {
        let mut budget = ContextBudget::from_config(self.context_window, self.reserved_tokens);
        budget.learn_provider_window(self.provider_window);
        budget
    }
}

/// `max_tokens == 0` means the output cap is unknown; then the configured
/// reserve stands, and only the window's own half-ceiling bounds it.
fn bound_reserve(reserved_tokens: u32, max_tokens: u32) -> u32 {
    match max_tokens {
        0 => reserved_tokens,
        cap => reserved_tokens.min(cap),
    }
}

/// The budget for callers outside a turn — the status bar and `/compact` —
/// so the reserve rule is applied in one place, not copied per call site.
pub fn budget_from_config(config: &crate::config::Config, provider_window: u32) -> ContextBudget {
    let mut budget = ContextBudget::from_config(
        config.context.context_window,
        bound_reserve(config.context.reserved_tokens, config.provider.max_tokens),
    );
    budget.learn_provider_window(provider_window);
    budget
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_spec_yields_no_budget() {
        assert!(ContextSpec::default().budget().snapshot(1_000).is_none());
    }

    #[test]
    fn provider_window_fills_in_when_config_is_silent() {
        let spec = ContextSpec {
            context_window: 0,
            provider_window: 100_000,
            reserved_tokens: 20_000,
            ..ContextSpec::default()
        };
        assert_eq!(spec.budget().usable(), Some(80_000));
    }

    #[test]
    fn max_tokens_bounds_the_reserve_of_a_turn() {
        let spec = ContextSpec {
            context_window: 0,
            provider_window: 100_000,
            reserved_tokens: 20_000,
            ..ContextSpec::default()
        }
        .with_output_cap(4_096);
        assert_eq!(spec.budget().usable(), Some(95_904));
    }

    #[test]
    fn an_unknown_output_cap_leaves_the_configured_reserve_alone() {
        let spec = ContextSpec {
            context_window: 100_000,
            reserved_tokens: 20_000,
            ..ContextSpec::default()
        }
        .with_output_cap(0);
        assert_eq!(spec.budget().usable(), Some(80_000));
    }

    #[test]
    fn a_turn_on_a_small_window_still_gets_a_budget() {
        let mut config = crate::config::Config::default();
        config.context.context_window = 8_000;
        config.context.reserved_tokens = 20_000;
        config.provider.max_tokens = 4_096;
        let spec = ContextSpec::new(&config.context, 0, "session")
            .with_output_cap(config.provider.max_tokens);
        // Without the bound the 20k reserve swallowed an 8k window whole and
        // the ladder switched itself off.
        assert_eq!(spec.budget().usable(), Some(4_000));
    }

    #[test]
    fn the_status_bar_path_applies_the_same_reserve_rule() {
        let mut config = crate::config::Config::default();
        config.context.context_window = 0;
        config.context.reserved_tokens = 20_000;
        config.provider.max_tokens = 4_096;
        // A large window: only the `max_tokens` bound moves this number.
        assert_eq!(budget_from_config(&config, 100_000).usable(), Some(95_904));
        // A small one: the `ctx:` indicator survives instead of vanishing.
        let small = budget_from_config(&config, 8_000);
        assert_eq!(small.usable(), Some(4_000));
        assert_eq!(
            small.snapshot(2_000).expect("known window").label(),
            "50% of 4k"
        );
    }
}
