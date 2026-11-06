use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/doctor` — откуда взялись значения конфига, где секреты, что не так.
pub struct DoctorCommand;

impl Command for DoctorCommand {
    fn name(&self) -> &str {
        "doctor"
    }

    fn description(&self) -> &str {
        "Where config values and secrets come from, and what is wrong"
    }

    fn usage(&self) -> &str {
        "/doctor"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, config: &Config) -> CommandResult {
        let config = config.clone();
        CommandResult::Detached(Box::new(move || crate::cli::doctor::text_for_tui(&config)))
    }
}
