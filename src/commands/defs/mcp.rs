use crate::app::AppState;
use crate::commands::{save_config_then, Command, CommandResult};
use crate::config::{self, Config};

pub struct McpCommand;

impl Command for McpCommand {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "Open MCP server management, set cache TTL, reload, authorize, or add a server"
    }

    fn usage(&self) -> &str {
        "/mcp — open management view\n/mcp ttl <secs> — set cache TTL (default: 300s)\n/mcp reload — force reload all servers\n/mcp auth (or /mcp oauth) — list servers requiring authorization\n/mcp add — add a server (paste JSON or step-by-step)\n/mcp add <name> <command> [args...] — quick stdio add\n/mcp add <json> — quick add from a pasted config"
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            state.view = crate::app::events::View::Mcp;
            state.mcp_status.view = crate::mcp::types::McpViewState {
                active: true,
                selected: 0,
                scroll_offset: 0,
                details_server: None,
                servers: Vec::new(),
                status_message: String::new(),
                auth_mode: false,
            };
            return CommandResult::Handled;
        }

        if args == "reload" {
            return CommandResult::ReloadMcp;
        }

        if args == "auth" || args == "oauth" {
            return CommandResult::OpenMcpAuth;
        }

        if args == "add" {
            return CommandResult::OpenMcpAdd(None);
        }
        if let Some(rest) = args.strip_prefix("add ") {
            return CommandResult::OpenMcpAdd(Some(rest.trim().to_string()));
        }

        if let Some(ttl_str) = args.strip_prefix("ttl ") {
            let trimmed = ttl_str.trim();
            match trimmed.parse::<u64>() {
                Ok(ttl) if ttl > 0 && ttl <= 86400 => {
                    let mut config = match config::load() {
                        Ok(c) => c,
                        Err(_) => return CommandResult::Error("Failed to load config".to_string()),
                    };
                    config.mcp.cache_ttl = ttl;
                    save_config_then(&config, || CommandResult::TtlUpdate(ttl))
                }
                Ok(_) => CommandResult::Error("TTL must be between 1 and 86400 seconds".to_string()),
                Err(_) => CommandResult::Error(
                    format!("Invalid TTL value: '{trimmed}'. Usage: /mcp ttl <seconds>")
                ),
            }
        } else {
            CommandResult::Error(
                "Unknown subcommand. Usage:\n  /mcp — open management view\n  /mcp ttl <secs> — set cache TTL\n  /mcp reload — force reload all servers\n  /mcp auth — list servers requiring authorization\n  /mcp add [args] — add a new server"
                    .to_string(),
            )
        }
    }
}
