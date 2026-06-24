pub mod onboarding;

use crate::config::Config;

pub fn should_run_onboarding(config: &Config) -> bool {
    config.provider.token.is_empty()
}
