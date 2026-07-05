//! `/serve` — API-server status / on / off / dialect, and `/server <port>`
//! — persist the listen port. Both only parse; the effects (spawn, shutdown,
//! config save, restart) live in `app/serve.rs` behind
//! `CommandResult::Serve`.

use crate::app::AppState;
use crate::commands::{Command, CommandResult, ServeAction};
use crate::config::{Config, ServerApi};

/// Parse `/serve` arguments. Pure — the commands below only wrap this.
fn parse_serve_args(args: &str) -> Result<ServeAction, String> {
    const USAGE: &str = "Usage: /serve [on|off|api <openai|anthropic|gemini>]";
    match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => Ok(ServeAction::Status),
        "on" | "start" => Ok(ServeAction::Start),
        "off" | "stop" => Ok(ServeAction::Stop),
        lower => match lower.strip_prefix("api") {
            Some(rest) => ServerApi::parse(rest)
                .map(ServeAction::SetApi)
                .ok_or_else(|| format!("Unknown API dialect '{}'. {USAGE}", rest.trim())),
            None => Err(USAGE.to_string()),
        },
    }
}

/// Parse `/server` arguments: empty = status, otherwise a port.
fn parse_server_args(args: &str) -> Result<ServeAction, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        // Bare `/server` reads as "tell me about the server".
        return Ok(ServeAction::Status);
    }
    match trimmed.parse::<u16>() {
        Ok(port) if port != 0 => Ok(ServeAction::SetPort(port)),
        _ => Err(format!(
            "'{trimmed}' is not a valid port (1-65535). Usage: /server <port>"
        )),
    }
}

fn to_result(parsed: Result<ServeAction, String>) -> CommandResult {
    match parsed {
        Ok(action) => CommandResult::Serve(action),
        Err(message) => CommandResult::Error(message),
    }
}

pub struct ServeCommand;

impl Command for ServeCommand {
    fn name(&self) -> &str {
        "serve"
    }

    fn description(&self) -> &str {
        "API server: status, on/off, wire dialect"
    }

    fn usage(&self) -> &str {
        "/serve [on|off|api <openai|anthropic|gemini>]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        to_result(parse_serve_args(args))
    }
}

pub struct ServerCommand;

impl Command for ServerCommand {
    fn name(&self) -> &str {
        "server"
    }

    fn description(&self) -> &str {
        "Set the API server port (persisted for future runs)"
    }

    fn usage(&self) -> &str {
        "/server <port>"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        to_result(parse_server_args(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_parses_status_on_off() {
        assert_eq!(parse_serve_args(""), Ok(ServeAction::Status));
        assert_eq!(parse_serve_args(" on "), Ok(ServeAction::Start));
        assert_eq!(parse_serve_args("OFF"), Ok(ServeAction::Stop));
        assert_eq!(parse_serve_args("stop"), Ok(ServeAction::Stop));
        assert!(parse_serve_args("sideways").is_err());
    }

    #[test]
    fn serve_parses_api_dialects() {
        assert_eq!(
            parse_serve_args("api openai"),
            Ok(ServeAction::SetApi(ServerApi::Openai))
        );
        assert_eq!(
            parse_serve_args("API Claude"),
            Ok(ServeAction::SetApi(ServerApi::Anthropic))
        );
        assert!(parse_serve_args("api soap").is_err());
        assert!(parse_serve_args("api").is_err(), "dialect name required");
    }

    #[test]
    fn server_parses_port_and_rejects_junk() {
        assert_eq!(parse_server_args("8080"), Ok(ServeAction::SetPort(8080)));
        assert_eq!(parse_server_args(""), Ok(ServeAction::Status));
        assert!(parse_server_args("0").is_err());
        assert!(parse_server_args("99999").is_err());
        assert!(parse_server_args("http").is_err());
    }
}
