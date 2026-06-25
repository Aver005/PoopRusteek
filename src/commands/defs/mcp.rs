use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::mcp::types::McpViewState;

pub struct McpCommand;

impl Command for McpCommand {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "Open MCP server management"
    }

    fn usage(&self) -> &str {
        "/mcp"
    }

    fn execute(&self, _args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        state.view = crate::app::events::View::Mcp;
        state.mcp_view = McpViewState {
            active: true,
            selected: 0,
            scroll_offset: 0,
            details_server: None,
            servers: Vec::new(),
            status_message: String::new(),
        };
        CommandResult::Handled
    }
}
