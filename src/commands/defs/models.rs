use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct ModelsCommand;

impl Command for ModelsCommand {
    fn name(&self) -> &str {
        "models"
    }

    fn description(&self) -> &str {
        "List the active provider's models, or switch to one directly"
    }

    fn usage(&self) -> &str {
        "/models [<model_id>]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            CommandResult::OpenModels
        } else {
            CommandResult::SwitchModel(args.to_string())
        }
    }
}
