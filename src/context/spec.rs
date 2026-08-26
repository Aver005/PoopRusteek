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
    /// Headroom subtracted from the window for the model's own answer.
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

    /// Config wins over the provider — `learn_provider_window` enforces that.
    pub fn budget(&self) -> ContextBudget {
        let mut budget = ContextBudget::from_config(self.context_window, self.reserved_tokens);
        budget.learn_provider_window(self.provider_window);
        budget
    }
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
}
