//! State machine + pure parsing helpers for `/mcp add`: paste a JSON server
//! config, walk a step-by-step wizard, or (from the command line) a quick
//! `<name> <command> [args...]` / inline-JSON shortcut. Kept as its own
//! module (like `mcp_status.rs`) so this flow's state and logic stay
//! cohesive instead of scattered across `keys.rs`.

use crate::app::input::InputState;
use crate::mcp::types::{MCPServerConfig, MCPServerStatus, McpServerDef};
use std::collections::HashMap;

/// Short human-readable label for a just-added server's connect outcome —
/// shown in the `/mcp add` result message.
pub fn status_label(status: &MCPServerStatus) -> String {
    match status {
        MCPServerStatus::Connected => "connected".to_string(),
        MCPServerStatus::AuthRequired(_) => "requires authorization \u{2014} use /mcp auth".to_string(),
        MCPServerStatus::Error(e) => format!("failed to connect: {e}"),
        MCPServerStatus::Pending | MCPServerStatus::Connecting => "connecting...".to_string(),
        MCPServerStatus::Disabled => "disabled".to_string(),
    }
}

/// Which entry method the user picked (or is currently walking through).
#[derive(Debug, Clone)]
pub enum McpAddState {
    /// `/mcp add` with no (usable) args: choose paste-JSON vs. the wizard.
    ChooseMethod { selected: usize },
    /// Free-form JSON paste box (multi-line via `InputState`).
    PasteJson { input: InputState, error: Option<String> },
    /// Step-by-step wizard. Boxed: `WizardState` (~376 bytes, 3 `InputState`
    /// fields) would otherwise make every `McpAddState` — including the
    /// tiny `ChooseMethod`/`PasteJson` variants — pay its size.
    Wizard(Box<WizardState>),
}

impl McpAddState {
    pub fn choose_method() -> Self {
        Self::ChooseMethod { selected: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportChoice {
    Stdio,
    Http,
    Sse,
}

impl TransportChoice {
    pub const ALL: [TransportChoice; 3] = [TransportChoice::Stdio, TransportChoice::Http, TransportChoice::Sse];

    pub fn label(self) -> &'static str {
        match self {
            TransportChoice::Stdio => "stdio (local command)",
            TransportChoice::Http => "http (remote URL)",
            TransportChoice::Sse => "sse (remote URL, streaming)",
        }
    }

    /// Prompt for the wizard's "primary value" step.
    pub fn primary_label(self) -> &'static str {
        match self {
            TransportChoice::Stdio => "Command (e.g. \"npx -y @scope/pkg\")",
            TransportChoice::Http | TransportChoice::Sse => "URL",
        }
    }

    /// Prompt for the wizard's optional "extra" step.
    pub fn extra_label(self) -> &'static str {
        match self {
            TransportChoice::Stdio => "Env vars, one KEY=VALUE per line (optional)",
            TransportChoice::Http | TransportChoice::Sse => "Headers, one \"Name: value\" per line (optional)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Name,
    Transport,
    Primary,
    Extra,
    Confirm,
}

#[derive(Debug, Clone)]
pub struct WizardState {
    pub step: WizardStep,
    pub name: InputState,
    pub transport_selected: usize,
    pub transport: Option<TransportChoice>,
    pub primary: InputState,
    pub extra: InputState,
    pub error: Option<String>,
}

impl WizardState {
    pub fn new() -> Self {
        Self {
            step: WizardStep::Name,
            name: InputState::default(),
            transport_selected: 0,
            transport: None,
            primary: InputState::default(),
            extra: InputState::default(),
            error: None,
        }
    }

    /// Build the final `MCPServerConfig` from the fields collected so far.
    /// Every field was already validated when the user advanced past its
    /// step, so this should only fail if that validation and this parsing
    /// somehow disagree — kept fallible rather than assumed-infallible.
    pub fn build_config(&self) -> Result<MCPServerConfig, String> {
        let transport = self.transport.ok_or_else(|| "no transport selected".to_string())?;
        match transport {
            TransportChoice::Stdio => {
                let (command, args) = split_command_line(self.primary.buffer.trim())?;
                let env = parse_env_lines(&self.extra.buffer)?;
                Ok(MCPServerConfig::Stdio {
                    command,
                    args,
                    env: if env.is_empty() { None } else { Some(env) },
                    cwd: None,
                })
            }
            TransportChoice::Http => Ok(MCPServerConfig::Http {
                url: self.primary.buffer.trim().to_string(),
                headers: parse_header_lines(&self.extra.buffer)?,
            }),
            TransportChoice::Sse => Ok(MCPServerConfig::Sse {
                url: self.primary.buffer.trim().to_string(),
                headers: parse_header_lines(&self.extra.buffer)?,
            }),
        }
    }
}

/// Split a shell-like command line into `(command, args)` by whitespace —
/// deliberately simple, no quoting support: covers the overwhelming
/// majority of MCP install snippets (`npx -y @scope/pkg`, `uvx some-tool`).
pub fn split_command_line(line: &str) -> Result<(String, Vec<String>), String> {
    let mut parts = line.split_whitespace();
    let command = parts.next().ok_or("command is empty")?.to_string();
    Ok((command, parts.map(str::to_string).collect()))
}

/// Parse `KEY=VALUE` lines (blank lines ignored) into an env map for a
/// stdio server. Empty input is a valid, empty map — env is optional.
pub fn parse_env_lines(text: &str) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| format!("line {}: expected KEY=VALUE", i + 1))?;
        let k = k.trim();
        if k.is_empty() {
            return Err(format!("line {}: empty key", i + 1));
        }
        map.insert(k.to_string(), v.trim().to_string());
    }
    Ok(map)
}

/// Parse `Name: value` lines (blank lines ignored) into a headers map for
/// an http/sse server. Empty input is a valid, empty map.
pub fn parse_header_lines(text: &str) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| format!("line {}: expected \"Name: value\"", i + 1))?;
        let k = k.trim();
        if k.is_empty() {
            return Err(format!("line {}: empty header name", i + 1));
        }
        map.insert(k.to_string(), v.trim().to_string());
    }
    Ok(map)
}

/// Parse a pasted server-config JSON blob. Accepts either the
/// Claude-Desktop-style wrapper `{"mcpServers": {"name": {...}}}` (adds
/// every entry — the common multi-server paste), or a single bare object
/// with an explicit `"name"` field bolted onto the usual `MCPServerConfig`
/// shape (`{"name": "x", "transport": "stdio", "command": "..."}`).
pub fn parse_pasted_json(raw: &str) -> Result<Vec<(String, MCPServerConfig)>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty input".to_string());
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON: {e}"))?;

    if let Some(servers) = value.get("mcpServers").and_then(|v| v.as_object()) {
        if servers.is_empty() {
            return Err("\"mcpServers\" object is empty".to_string());
        }
        let mut out = Vec::with_capacity(servers.len());
        for (name, def) in servers {
            let def: McpServerDef = serde_json::from_value(def.clone()).map_err(|e| format!("server '{name}': {e}"))?;
            out.push((name.clone(), def.config));
        }
        return Ok(out);
    }

    let name = value.get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "expected a top-level \"mcpServers\" object, or a \"name\" field".to_string())?
        .to_string();
    let config: MCPServerConfig = serde_json::from_value(value).map_err(|e| format!("invalid server config: {e}"))?;
    Ok(vec![(name, config)])
}

/// Parse `/mcp add <args>`'s quick inline syntax: the same JSON shapes
/// `parse_pasted_json` accepts (if `args` looks like JSON), or
/// `<name> <command> [args...]` for a fast stdio add — by far the most
/// common case (`npx -y @scope/pkg`, no env/headers).
pub fn parse_quick_add(raw: &str) -> Result<Vec<(String, MCPServerConfig)>, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return parse_pasted_json(trimmed);
    }
    let mut parts = trimmed.split_whitespace();
    let name = parts.next().ok_or("expected \"<name> <command> [args...]\" or a JSON config")?.to_string();
    let command = parts.next().ok_or("missing command \u{2014} expected \"<name> <command> [args...]\"")?.to_string();
    let args: Vec<String> = parts.map(str::to_string).collect();
    Ok(vec![(name, MCPServerConfig::Stdio { command, args, env: None, cwd: None })])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_covers_every_variant() {
        assert_eq!(status_label(&MCPServerStatus::Connected), "connected");
        assert_eq!(status_label(&MCPServerStatus::Disabled), "disabled");
        assert!(status_label(&MCPServerStatus::AuthRequired(None)).contains("authorization"));
        assert!(status_label(&MCPServerStatus::Error("boom".to_string())).contains("boom"));
    }

    #[test]
    fn split_command_line_splits_command_and_args() {
        let (cmd, args) = split_command_line("npx -y @scope/pkg").unwrap();
        assert_eq!(cmd, "npx");
        assert_eq!(args, vec!["-y".to_string(), "@scope/pkg".to_string()]);
    }

    #[test]
    fn split_command_line_no_args_is_fine() {
        let (cmd, args) = split_command_line("uvx-tool").unwrap();
        assert_eq!(cmd, "uvx-tool");
        assert!(args.is_empty());
    }

    #[test]
    fn split_command_line_empty_is_error() {
        assert!(split_command_line("   ").is_err());
    }

    #[test]
    fn parse_env_lines_parses_key_value_pairs() {
        let map = parse_env_lines("FOO=bar\nBAZ=qux quux\n").unwrap();
        assert_eq!(map.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(map.get("BAZ").map(String::as_str), Some("qux quux"));
    }

    #[test]
    fn parse_env_lines_ignores_blank_lines() {
        let map = parse_env_lines("\nFOO=bar\n\n").unwrap();
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn parse_env_lines_empty_input_is_empty_map() {
        assert!(parse_env_lines("").unwrap().is_empty());
        assert!(parse_env_lines("   \n  \n").unwrap().is_empty());
    }

    #[test]
    fn parse_env_lines_rejects_missing_equals() {
        assert!(parse_env_lines("NOTKEYVALUE").is_err());
    }

    #[test]
    fn parse_env_lines_rejects_empty_key() {
        assert!(parse_env_lines("=value").is_err());
    }

    #[test]
    fn parse_header_lines_parses_colon_pairs() {
        let map = parse_header_lines("Authorization: Bearer xyz\nX-Custom: 1").unwrap();
        assert_eq!(map.get("Authorization").map(String::as_str), Some("Bearer xyz"));
        assert_eq!(map.get("X-Custom").map(String::as_str), Some("1"));
    }

    #[test]
    fn parse_header_lines_rejects_missing_colon() {
        assert!(parse_header_lines("NoColonHere").is_err());
    }

    #[test]
    fn parse_pasted_json_single_object_with_name() {
        let json = r#"{"name": "fetch", "transport": "stdio", "command": "npx", "args": ["-y", "@x/fetch"]}"#;
        let entries = parse_pasted_json(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "fetch");
        match &entries[0].1 {
            MCPServerConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["-y".to_string(), "@x/fetch".to_string()]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn parse_pasted_json_mcp_servers_wrapper_multiple_entries() {
        let json = r#"{
            "mcpServers": {
                "fetch": {"transport": "stdio", "command": "npx", "args": ["-y", "@x/fetch"]},
                "notion": {"transport": "http", "url": "https://example.com/mcp"}
            }
        }"#;
        let mut entries = parse_pasted_json(json).unwrap();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "fetch");
        assert_eq!(entries[1].0, "notion");
    }

    #[test]
    fn parse_pasted_json_rejects_invalid_json() {
        assert!(parse_pasted_json("not json").is_err());
    }

    #[test]
    fn parse_pasted_json_rejects_missing_name_and_wrapper() {
        let json = r#"{"transport": "stdio", "command": "npx"}"#;
        assert!(parse_pasted_json(json).is_err());
    }

    #[test]
    fn parse_pasted_json_rejects_empty_mcp_servers_object() {
        assert!(parse_pasted_json(r#"{"mcpServers": {}}"#).is_err());
    }

    #[test]
    fn parse_quick_add_name_command_args_shorthand() {
        let entries = parse_quick_add("fetch npx -y @x/fetch").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "fetch");
        match &entries[0].1 {
            MCPServerConfig::Stdio { command, args, env, cwd } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["-y".to_string(), "@x/fetch".to_string()]);
                assert!(env.is_none());
                assert!(cwd.is_none());
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn parse_quick_add_delegates_to_json_parser_when_it_looks_like_json() {
        let json = r#"{"name": "fetch", "transport": "http", "url": "https://x/mcp"}"#;
        let entries = parse_quick_add(json).unwrap();
        assert_eq!(entries[0].0, "fetch");
    }

    #[test]
    fn parse_quick_add_rejects_missing_command() {
        assert!(parse_quick_add("just-a-name").is_err());
    }

    #[test]
    fn parse_quick_add_rejects_empty_input() {
        assert!(parse_quick_add("   ").is_err());
    }
}
