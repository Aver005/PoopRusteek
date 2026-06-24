pub mod onboarding;
pub mod file_mentions;

use crate::config::Config;

pub fn should_run_onboarding(config: &Config) -> bool {
    config.provider.token.is_empty()
}
