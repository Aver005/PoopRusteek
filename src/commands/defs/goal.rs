use crate::app::events::GoalStage;
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
        state.goal.mode = !state.goal.mode;

        if state.goal.mode {
            state.goal.stage = GoalStage::Inactive;
            state.goal.prompt.clear();
            state.goal.text.clear();
            state.goal.iteration = 0;
            state.goal.agent1_failures = 0;
            state.goal.agent2_failures = 0;
            state.goal.summary.clear();
            state.goal.agent1_session_id = crate::session::create_session_id();
            state.goal.agent2_session_id = crate::session::create_session_id();
            state.messages.push(crate::provider::ChatMessage::system(
                "GOAL mode activated. Enter your prompt, then define what goal must be achieved.",
            ));
        } else {
            state.goal.stage = GoalStage::Inactive;
            state.goal.prompt.clear();
            state.goal.text.clear();
            state.goal.iteration = 0;
            state.goal.agent1_failures = 0;
            state.goal.agent2_failures = 0;
            state.goal.summary.clear();
            state.messages.push(crate::provider::ChatMessage::system(
                "GOAL mode deactivated.",
            ));
        }

        CommandResult::Handled
    }
}
