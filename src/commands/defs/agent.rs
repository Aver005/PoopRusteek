use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/agent <prompt>` — launch an isolated sub-agent (own forked session,
/// auto-approved tools) in the background. Its result is delivered into the
/// chat when it finishes; manage running ones with `/agents`.
pub struct AgentCommand;

impl Command for AgentCommand {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Launch a background sub-agent for a task"
    }

    fn usage(&self) -> &str {
        "/agent <task>"
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let prompt = args.trim();
        if prompt.is_empty() {
            state.messages.push(crate::provider::ChatMessage::system(
                "Usage: /agent <task>",
            ));
            return CommandResult::Handled;
        }
        CommandResult::SpawnAgent(prompt.to_string())
    }
}

/// `/agents` — list running sub-agents / sidechats and stop them.
pub struct AgentsCommand;

impl Command for AgentsCommand {
    fn name(&self) -> &str {
        "agents"
    }

    fn description(&self) -> &str {
        "List and stop running background agents"
    }

    fn usage(&self) -> &str {
        "/agents"
    }

    fn execute(&self, _args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        CommandResult::OpenAgents
    }
}
