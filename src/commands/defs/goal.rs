use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct GoalCommand;

impl Command for GoalCommand {
    fn name(&self) -> &str {
        "/goal"
    }

    fn description(&self) -> &str {
        "Toggle GOAL mode: define a goal and iterate until achieved"
    }

    fn usage(&self) -> &str {
        "/goal"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        if state.goal.mode {
            state.goal.deactivate();
            state.messages.push(crate::provider::ChatMessage::system(
                "GOAL mode deactivated.",
            ));
        } else {
            state.goal.activate();
            state.messages.push(crate::provider::ChatMessage::system(
                "GOAL mode activated. Enter your prompt, then define what goal must be achieved.",
            ));
        }

        CommandResult::Handled
    }
}
