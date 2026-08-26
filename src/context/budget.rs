use crate::provider::ChatMessage;

/// Chars per token for budget maths. Three, not the usual four: this estimate
/// must overshoot, because a threshold that fires late fires after the
/// provider already refused. Cyrillic and code both run under four.
const CHARS_PER_TOKEN: u32 = 3;

/// Per-message wire overhead (role, separators, JSON punctuation).
const PER_MESSAGE_OVERHEAD: u32 = 4;

/// Deliberately pessimistic token estimate — see `CHARS_PER_TOKEN`. Not a
/// tokenizer: use it for thresholds, never to report a token count.
pub fn budget_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(CHARS_PER_TOKEN)
}

/// What the whole conversation costs on the wire. `ui_only` messages are
/// excluded because they never reach the provider. O(n) over every message —
/// keep it off the event loop on long histories (invariant 9).
pub fn conversation_tokens(system_prompt: &str, messages: &[ChatMessage]) -> u32 {
    let mut total = budget_tokens(system_prompt).saturating_add(PER_MESSAGE_OVERHEAD);
    for message in messages.iter().filter(|message| !message.ui_only) {
        total = total
            .saturating_add(budget_tokens(&message.content))
            .saturating_add(PER_MESSAGE_OVERHEAD);
        if let Some(name) = &message.name {
            total = total.saturating_add(budget_tokens(name));
        }
    }
    total
}

/// Where the window size came from. Reported to the user because a guessed
/// window and a known one deserve different trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// `[context] context_window` — the user said so, nobody overrides it.
    Config,
    /// Answered by the provider's own model listing.
    Provider,
    /// Nobody knows. Compaction stays off (invariant 12).
    Unknown,
}

/// The window and the headroom reserved from it. Holds no conversation state:
/// callers pass the current usage in.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudget {
    window: u32,
    source: WindowSource,
    reserved: u32,
}

impl ContextBudget {
    /// `context_window == 0` means "unknown, ask the provider later".
    pub fn from_config(context_window: u32, reserved_tokens: u32) -> Self {
        let (window, source) = match context_window {
            0 => (0, WindowSource::Unknown),
            explicit => (explicit, WindowSource::Config),
        };
        Self {
            window,
            source,
            reserved: reserved_tokens,
        }
    }

    /// Fill in a window the provider reported. A window set in config wins —
    /// the user overrides the catalogue, never the other way round.
    pub fn learn_provider_window(&mut self, window: u32) {
        if self.source == WindowSource::Config || window == 0 {
            return;
        }
        self.window = window;
        self.source = WindowSource::Provider;
    }

    /// Room for the conversation once the model's own answer is reserved.
    /// `None` whenever the ladder must stay off: unknown window, or a reserve
    /// that swallows it whole.
    pub fn usable(&self) -> Option<u32> {
        if self.source == WindowSource::Unknown || self.window == 0 {
            return None;
        }
        self.window
            .checked_sub(self.reserved)
            .filter(|left| *left > 0)
    }

    pub fn snapshot(&self, used: u32) -> Option<BudgetSnapshot> {
        Some(BudgetSnapshot {
            window: self.window,
            usable: self.usable()?,
            used,
            source: self.source,
        })
    }
}

/// One reading of how full the window is.
#[derive(Debug, Clone, Copy)]
pub struct BudgetSnapshot {
    pub window: u32,
    pub usable: u32,
    pub used: u32,
    pub source: WindowSource,
}

impl BudgetSnapshot {
    pub fn percent_used(&self) -> u8 {
        if self.usable == 0 {
            return 100;
        }
        let percent = (u64::from(self.used) * 100) / u64::from(self.usable);
        percent.min(100) as u8
    }

    /// Compact status-bar form. A hand-set window is marked `*`: it is the one
    /// case where the number can disagree with the model's real limit.
    pub fn label(&self) -> String {
        let hand_set = if self.source == WindowSource::Config {
            "*"
        } else {
            ""
        };
        format!(
            "{}% of {}k{}",
            self.percent_used(),
            self.window.div_ceil(1_000),
            hand_set
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    #[test]
    fn budget_tokens_rounds_up_and_counts_chars_not_bytes() {
        assert_eq!(budget_tokens(""), 0);
        assert_eq!(budget_tokens("abcd"), 2);
        // Cyrillic is 2 bytes per char: counting bytes would double this.
        assert_eq!(budget_tokens("абвабв"), 2);
    }

    #[test]
    fn conversation_tokens_skips_ui_only_messages() {
        let mut chrome = ChatMessage::system("this notice never reaches the provider");
        chrome.ui_only = true;
        let messages = vec![ChatMessage::user("hello"), chrome];
        let with_chrome = conversation_tokens("sys", &messages);
        let without_chrome = conversation_tokens("sys", &messages[..1]);
        assert_eq!(with_chrome, without_chrome);
    }

    #[test]
    fn unknown_window_yields_no_budget_so_the_ladder_stays_off() {
        let budget = ContextBudget::from_config(0, 20_000);
        assert!(budget.usable().is_none());
        assert!(budget.snapshot(1_000).is_none());
    }

    #[test]
    fn reserve_larger_than_window_yields_no_budget() {
        let budget = ContextBudget::from_config(8_000, 20_000);
        assert!(budget.usable().is_none());
    }

    #[test]
    fn config_window_outranks_the_provider() {
        let mut budget = ContextBudget::from_config(32_000, 2_000);
        budget.learn_provider_window(128_000);
        assert_eq!(budget.usable(), Some(30_000));
        // The `*` marks it as hand-set: the provider did not overwrite it.
        assert!(
            budget
                .snapshot(0)
                .expect("known window")
                .label()
                .ends_with('*')
        );
    }

    #[test]
    fn provider_window_fills_in_when_config_is_silent() {
        let mut budget = ContextBudget::from_config(0, 2_000);
        budget.learn_provider_window(128_000);
        assert_eq!(budget.usable(), Some(126_000));
        assert!(
            !budget
                .snapshot(0)
                .expect("known window")
                .label()
                .ends_with('*')
        );
    }

    #[test]
    fn snapshot_reports_fullness() {
        let budget = ContextBudget::from_config(10_000, 2_000);
        let snapshot = budget.snapshot(4_000).expect("known window");
        assert_eq!(snapshot.usable, 8_000);
        assert_eq!(snapshot.percent_used(), 50);
    }

    #[test]
    fn label_marks_a_hand_set_window() {
        let hand_set = ContextBudget::from_config(128_000, 20_000)
            .snapshot(54_000)
            .expect("known window");
        assert_eq!(hand_set.label(), "50% of 128k*");

        let mut learned = ContextBudget::from_config(0, 20_000);
        learned.learn_provider_window(32_000);
        assert_eq!(
            learned.snapshot(6_000).expect("known window").label(),
            "50% of 32k"
        );
    }
}
