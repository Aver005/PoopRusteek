//! Discovery and persistence of MCP server configuration.
//!
//! Servers can come from two kinds of place: our own config file
//! (`own_config_path()`, `mcpServers` key — full config we own and can
//! freely rewrite) or a foreign tool's config (Claude Desktop, VS Code,
//! Cursor, opencode, the Claude CLI, or workspace files — read-only sources
//! we merge in for convenience but never write to, and never fully persist
//! into our own file). `load_mcp_config` merges everything into one map for
//! `MCPManager` to connect to; `load_own_enabled_map`'s key set is how
//! `MCPManager` tells the two kinds apart afterward (a name present there
//! is ours), which matters for `save_mcp_config` — see its doc comment.

use super::types::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A minimal per-name override persisted for a *foreign* server (see module
/// docs): its enabled/disabled state only, never its command/args/env. This
/// is what lets toggling a Claude-Desktop-discovered server off/on survive a
/// restart without ever copying that server's config — and its secrets —
/// into our own `mcp.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerOverride {
    enabled: bool,
}

/// Shape of our own `mcp.json`. `overrides` is new; `#[serde(default)]`
/// keeps existing files (written before overrides existed) parseable.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OwnConfigFile {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerDef>,
    #[serde(default)]
    overrides: HashMap<String, ServerOverride>,
}

/// Read and parse our own config file, if it exists and parses. The single
/// shared entry point for all readers below — previously each of
/// `load_own_config` and `load_own_enabled_map` re-declared and re-parsed
/// against their own private copy of this shape.
fn load_own_config_file() -> Option<OwnConfigFile> {
    let path = own_config_path();
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

#[derive(Debug, Deserialize)]
struct ClaudeDesktopConfig {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, ClaudeDesktopServer>,
}

#[derive(Debug, Deserialize)]
struct ClaudeDesktopServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct VSCodeMCPServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
}

pub fn load_mcp_config() -> HashMap<String, MCPServerConfig> {
    let mut configs = HashMap::new();

    load_own_config(&mut configs);
    load_workspace_config(&mut configs);
    load_global_config(&mut configs);
    load_opencode_config(&mut configs);
    load_claude_cli_config(&mut configs);
    load_claude_desktop_config(&mut configs);
    load_vscode_config(&mut configs);
    load_cursor_config(&mut configs);

    configs
}

pub fn own_config_path() -> PathBuf {
    crate::config::Config::data_dir().join("mcp.json")
}

/// Enabled/disabled state for every server in our own `mcpServers` section
/// (i.e. servers we fully own). Used both to apply saved enabled-state at
/// startup and to distinguish "own" from "foreign" servers in `MCPManager`
/// (a name present here is ours).
pub fn load_own_enabled_map() -> HashMap<String, bool> {
    let Some(parsed) = load_own_config_file() else {
        return HashMap::new();
    };
    parsed
        .mcp_servers
        .into_iter()
        .map(|(k, v)| (k, v.enabled))
        .collect()
}

/// Enabled-state overrides for *foreign* servers (see module docs) that
/// were toggled in-app. Unlike `load_own_enabled_map`, this never carries a
/// full `MCPServerConfig` — only the bool — because we never want to (and
/// per `save_mcp_config`'s contract, never do) copy a foreign server's
/// command/env into our own file.
pub fn load_overrides() -> HashMap<String, bool> {
    let Some(parsed) = load_own_config_file() else {
        return HashMap::new();
    };
    parsed
        .overrides
        .into_iter()
        .map(|(k, v)| (k, v.enabled))
        .collect()
}

fn load_own_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let Some(parsed) = load_own_config_file() else {
        return;
    };
    for (name, def) in parsed.mcp_servers {
        if def.enabled {
            configs.entry(name).or_insert(def.config);
        }
    }
}

/// Persist our own MCP config.
///
/// `own_servers` is written verbatim into `mcpServers` — full config
/// (command/args/env included) — and must contain **only** servers that
/// originated from our own config file or were added/edited in-app.
/// `foreign_overrides` is written into a separate `overrides` section as
/// name -> enabled bool only.
///
/// This split is the fix for a config leak: servers discovered from Claude
/// Desktop, VS Code, or Cursor configs (which can carry API keys/tokens in
/// their `env` maps) must never be copied into our own `mcp.json` just
/// because the user toggled them on/off in our UI — callers (`MCPManager`)
/// are responsible for routing each server into the right bucket before
/// calling this; this function does not re-derive source itself.
pub fn save_mcp_config(
    own_servers: &HashMap<String, (MCPServerConfig, bool)>,
    foreign_overrides: &HashMap<String, bool>,
) -> crate::error::AppResult<()> {
    let mcp_servers = own_servers
        .iter()
        .map(|(name, (config, enabled))| {
            (
                name.clone(),
                McpServerDef {
                    enabled: *enabled,
                    config: config.clone(),
                },
            )
        })
        .collect();
    let overrides = foreign_overrides
        .iter()
        .map(|(name, enabled)| (name.clone(), ServerOverride { enabled: *enabled }))
        .collect();
    let own = OwnConfigFile {
        mcp_servers,
        overrides,
    };
    let content = serde_json::to_string_pretty(&own)
        .map_err(|e| crate::error::AppError::Config(e.to_string()))?;
    let path = own_config_path();
    crate::util::atomic_write(&path, content.as_bytes())
        .map_err(|e| crate::error::AppError::Config(e.to_string()))
}

/// Shared skeleton for the per-source loaders below: if `path` exists, read
/// it, run `parse` over the contents, and merge the result via
/// `merge_parsed`. Read and parse failures are silently skipped, exactly as
/// each loader previously did inline. Returns whether the path existed at
/// all, so loaders that probe an ordered candidate list (claude-cli) can
/// stop at the first file found even when it fails to parse.
fn load_from_path(
    path: &Path,
    parse: fn(&str) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error>,
    configs: &mut HashMap<String, MCPServerConfig>,
) -> bool {
    if !path.exists() {
        return false;
    }
    if let Ok(content) = std::fs::read_to_string(path)
        && let Ok(parsed) = parse(&content)
    {
        merge_parsed(parsed, configs);
    }
    true
}

/// Merge freshly parsed servers into the accumulated map.
/// `entry().or_insert` means the first source to claim a name wins —
/// precedence is purely the call order in `load_mcp_config`.
fn merge_parsed(
    parsed: HashMap<String, MCPServerConfig>,
    configs: &mut HashMap<String, MCPServerConfig>,
) {
    for (name, config) in parsed {
        configs.entry(name).or_insert(config);
    }
}

fn load_workspace_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let paths = [
        PathBuf::from("mcp.config.json"),
        PathBuf::from(".vscode/mcp.json"),
    ];

    for path in &paths {
        load_from_path(path, parse_mcp_json, configs);
    }
}

fn load_global_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pooprusteek")
        .join("mcp.config.json");

    load_from_path(&config_dir, parse_mcp_json, configs);
}

fn load_claude_desktop_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let config_path = if cfg!(target_os = "macos") {
        dirs::home_dir()
            .map(|h| h.join("Library/Application Support/Claude/claude_desktop_config.json"))
    } else {
        // Windows and Linux both keep it under the platform config dir.
        dirs::config_dir().map(|c| c.join("Claude/claude_desktop_config.json"))
    };

    if let Some(path) = config_path {
        load_from_path(&path, parse_claude_desktop_json, configs);
    }
}

/// Parse Claude Desktop's `claude_desktop_config.json` shape — always stdio
/// (`command`/`args`/`env`, no url variant) under an `mcpServers` key.
fn parse_claude_desktop_json(
    content: &str,
) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
    let desktop_config: ClaudeDesktopConfig = serde_json::from_str(content)?;
    Ok(desktop_config
        .mcp_servers
        .into_iter()
        .map(|(name, server)| {
            let config = MCPServerConfig::Stdio {
                command: server.command,
                args: server.args,
                env: server.env,
                cwd: None,
            };
            (name, config)
        })
        .collect())
}

fn load_vscode_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let settings_path = dirs::config_dir().map(|c| c.join("Code/User/settings.json"));

    if let Some(path) = settings_path {
        load_from_path(&path, parse_vscode_settings, configs);
    }
}

/// Parse the `mcp.servers` section of VS Code's `settings.json`. Servers
/// with a `url` become HTTP, otherwise stdio; entries that don't match the
/// expected shape are skipped individually rather than failing the file.
fn parse_vscode_settings(
    content: &str,
) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
    let settings: serde_json::Value = serde_json::from_str(content)?;
    let mut configs = HashMap::new();

    if let Some(mcp) = settings.get("mcp")
        && let Some(servers) = mcp.get("servers")
        && let Some(obj) = servers.as_object()
    {
        for (name, server_value) in obj {
            if let Ok(server) = serde_json::from_value::<VSCodeMCPServer>(server_value.clone()) {
                let config = if let Some(url) = &server.url {
                    MCPServerConfig::Http {
                        url: url.clone(),
                        headers: server.headers.unwrap_or_default(),
                    }
                } else {
                    MCPServerConfig::Stdio {
                        command: server.command,
                        args: server.args,
                        env: server.env,
                        cwd: None,
                    }
                };
                configs.insert(name.clone(), config);
            }
        }
    }

    Ok(configs)
}

fn load_cursor_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let cursor_path = dirs::home_dir().map(|h| h.join(".cursor/mcp.json"));

    if let Some(path) = cursor_path {
        load_from_path(&path, parse_mcp_json, configs);
    }
}

fn load_opencode_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let base = dirs::home_dir().map(|h| h.join(".config").join("opencode"));
    let Some(base) = base else { return };

    // Check opencode.json (canonical), then other possible names
    let candidates = [
        base.join("opencode.json"),
        base.join("opencode.jsonc"),
        base.join("mcp.json"),
        base.join("mcp.config.json"),
        base.join("opencode.mcp.json"),
    ];

    // Quirk that keeps this loader off `load_from_path`: the first existing
    // candidate is tried with *two* parsers, so the file is read once here
    // and only the merge step is shared.
    for path in &candidates {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                // try opencode format first, fall back to standard mcp format
                let is_opencode = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("opencode"))
                    .unwrap_or(false);

                if is_opencode && let Ok(parsed) = parse_opencode_json(&content) {
                    merge_parsed(parsed, configs);
                    return;
                }
                // fall through to standard parser if opencode format fails

                if let Ok(parsed) = parse_mcp_json(&content) {
                    merge_parsed(parsed, configs);
                }
            }
            return;
        }
    }
}

fn parse_opencode_json(
    content: &str,
) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(content)?;
    let mut configs = HashMap::new();

    if let Some(mcp) = value.get("mcp")
        && let Some(obj) = mcp.as_object()
    {
        for (name, server) in obj {
            // skip disabled servers
            if server.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }

            let config = match server.get("type").and_then(|t| t.as_str()) {
                Some("remote") => {
                    let url = server
                        .get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .to_string();
                    let headers = server
                        .get("headers")
                        .and_then(|h| serde_json::from_value(h.clone()).ok())
                        .unwrap_or_default();
                    match server.get("subtype").and_then(|t| t.as_str()) {
                        Some("sse") => MCPServerConfig::Sse { url, headers },
                        _ => MCPServerConfig::Http { url, headers },
                    }
                }
                Some("local") => {
                    let (command, args) =
                        if let Some(cmd_arr) = server.get("command").and_then(|c| c.as_array()) {
                            let mut parts: Vec<String> = cmd_arr
                                .iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect();
                            let cmd = if parts.is_empty() {
                                String::new()
                            } else {
                                parts.remove(0)
                            };
                            (cmd, parts)
                        } else {
                            (String::new(), vec![])
                        };
                    let env = server
                        .get("env")
                        .and_then(|e| serde_json::from_value(e.clone()).ok());
                    MCPServerConfig::Stdio {
                        command,
                        args,
                        env,
                        cwd: None,
                    }
                }
                _ => continue,
            };
            configs.insert(name.clone(), config);
        }
    }

    Ok(configs)
}

fn load_claude_cli_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let candidates = [
        // ~/.claude/mcp.json (standard Claude CLI location)
        dirs::home_dir().map(|h| h.join(".claude").join("mcp.json")),
        // Also check XDG-like ~/.config/claude/mcp.json
        dirs::config_dir().map(|c| c.join("claude").join("mcp.json")),
    ];

    for candidate in candidates.iter().flatten() {
        if load_from_path(candidate, parse_mcp_json, configs) {
            // take the first found file only
            return;
        }
    }
}

fn parse_mcp_json(content: &str) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(content)?;
    let mut configs = HashMap::new();

    if let Some(servers) = value.get("mcpServers").or_else(|| value.get("servers"))
        && let Some(obj) = servers.as_object()
    {
        for (name, server) in obj {
            let config = if let Some(url) = server.get("url").and_then(|u| u.as_str()) {
                let headers = server
                    .get("headers")
                    .and_then(|h| serde_json::from_value(h.clone()).ok())
                    .unwrap_or_default();
                let is_sse = server
                    .get("transport")
                    .and_then(|t| t.as_str())
                    .map(|t| t.eq_ignore_ascii_case("sse"))
                    .unwrap_or(false);
                if is_sse {
                    MCPServerConfig::Sse {
                        url: url.to_string(),
                        headers,
                    }
                } else {
                    MCPServerConfig::Http {
                        url: url.to_string(),
                        headers,
                    }
                }
            } else {
                let command = server
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = server
                    .get("args")
                    .and_then(|a| serde_json::from_value(a.clone()).ok())
                    .unwrap_or_default();
                let env = server
                    .get("env")
                    .and_then(|e| serde_json::from_value(e.clone()).ok());
                MCPServerConfig::Stdio {
                    command,
                    args,
                    env,
                    cwd: None,
                }
            };
            configs.insert(name.clone(), config);
        }
    }

    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_config_file_parses_old_format_without_overrides() {
        // Files written before `overrides` existed have no such key at all —
        // `#[serde(default)]` must make this still parse rather than reject
        // the whole file.
        let json = r#"{
            "mcpServers": {
                "filesystem": {
                    "enabled": true,
                    "transport": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"]
                }
            }
        }"#;
        let parsed: OwnConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.mcp_servers.len(), 1);
        assert!(parsed.mcp_servers["filesystem"].enabled);
        assert!(parsed.overrides.is_empty());
    }

    #[test]
    fn own_config_file_round_trips_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "claude-desktop-server".to_string(),
            ServerOverride { enabled: false },
        );
        let file = OwnConfigFile {
            mcp_servers: HashMap::new(),
            overrides,
        };
        let json = serde_json::to_string(&file).unwrap();
        let parsed: OwnConfigFile = serde_json::from_str(&json).unwrap();
        assert!(parsed.mcp_servers.is_empty());
        assert_eq!(parsed.overrides.len(), 1);
        assert!(!parsed.overrides["claude-desktop-server"].enabled);
    }

    #[test]
    fn save_mcp_config_never_mixes_own_and_foreign_shapes() {
        // Regression guard for the leak this type split fixes: a foreign
        // server's env (which may hold secrets) must never be reachable
        // from `mcp_servers` — `foreign_overrides` is typed as `bool` only,
        // so there is no field to accidentally serialize it into.
        let mut own = HashMap::new();
        own.insert(
            "our-server".to_string(),
            (
                MCPServerConfig::Stdio {
                    command: "our-cmd".to_string(),
                    args: vec![],
                    env: None,
                    cwd: None,
                },
                true,
            ),
        );
        let mcp_servers: HashMap<String, McpServerDef> = own
            .iter()
            .map(|(name, (config, enabled))| {
                (
                    name.clone(),
                    McpServerDef {
                        enabled: *enabled,
                        config: config.clone(),
                    },
                )
            })
            .collect();
        let mut foreign_overrides = HashMap::new();
        foreign_overrides.insert("claude-desktop-server".to_string(), false);
        let overrides: HashMap<String, ServerOverride> = foreign_overrides
            .iter()
            .map(|(name, enabled)| (name.clone(), ServerOverride { enabled: *enabled }))
            .collect();
        let file = OwnConfigFile {
            mcp_servers,
            overrides,
        };
        let json = serde_json::to_string(&file).unwrap();

        assert!(json.contains("our-server"));
        assert!(json.contains("our-cmd"));
        assert!(json.contains("claude-desktop-server"));
        // The foreign server name appears (as an override key) but nothing
        // that looks like a command/secret payload for it does.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let foreign_entry = &value["overrides"]["claude-desktop-server"];
        assert!(foreign_entry.get("command").is_none());
        assert!(foreign_entry.get("env").is_none());
        assert_eq!(foreign_entry["enabled"], false);
    }

    #[test]
    fn parse_mcp_json_stdio_server() {
        let json = r#"{
            "mcpServers": {
                "fs": {
                    "command": "npx",
                    "args": ["-y", "server-fs"],
                    "env": {"KEY": "value"}
                }
            }
        }"#;
        let configs = parse_mcp_json(json).unwrap();
        match &configs["fs"] {
            MCPServerConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["-y".to_string(), "server-fs".to_string()]);
                assert_eq!(env.as_ref().unwrap().get("KEY").unwrap(), "value");
                assert!(cwd.is_none());
            }
            other => panic!("expected Stdio config, got {other:?}"),
        }
    }

    #[test]
    fn parse_mcp_json_http_server_via_url() {
        let json = r#"{
            "mcpServers": {
                "remote": { "url": "https://example.com/mcp" }
            }
        }"#;
        let configs = parse_mcp_json(json).unwrap();
        match &configs["remote"] {
            MCPServerConfig::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert!(headers.is_empty());
            }
            other => panic!("expected Http config, got {other:?}"),
        }
    }

    #[test]
    fn parse_mcp_json_sse_server_via_transport_field() {
        let json = r#"{
            "mcpServers": {
                "remote": { "url": "https://example.com/mcp", "transport": "SSE" }
            }
        }"#;
        let configs = parse_mcp_json(json).unwrap();
        match &configs["remote"] {
            MCPServerConfig::Sse { url, .. } => assert_eq!(url, "https://example.com/mcp"),
            other => panic!("expected Sse config, got {other:?}"),
        }
    }

    #[test]
    fn parse_mcp_json_accepts_servers_key_alias() {
        // Some tools use "servers" instead of "mcpServers".
        let json = r#"{ "servers": { "fs": { "command": "npx" } } }"#;
        let configs = parse_mcp_json(json).unwrap();
        assert!(configs.contains_key("fs"));
    }

    #[test]
    fn parse_mcp_json_empty_object_yields_no_servers() {
        let configs = parse_mcp_json("{}").unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn parse_mcp_json_invalid_json_errors() {
        assert!(parse_mcp_json("not json").is_err());
    }

    #[test]
    fn parse_opencode_json_local_server() {
        let json = r#"{
            "mcp": {
                "local-tool": {
                    "type": "local",
                    "command": ["node", "server.js"],
                    "env": {"FOO": "bar"}
                }
            }
        }"#;
        let configs = parse_opencode_json(json).unwrap();
        match &configs["local-tool"] {
            MCPServerConfig::Stdio {
                command, args, env, ..
            } => {
                assert_eq!(command, "node");
                assert_eq!(args, &vec!["server.js".to_string()]);
                assert_eq!(env.as_ref().unwrap().get("FOO").unwrap(), "bar");
            }
            other => panic!("expected Stdio config, got {other:?}"),
        }
    }

    #[test]
    fn parse_opencode_json_remote_http_server() {
        let json = r#"{
            "mcp": {
                "remote-tool": { "type": "remote", "url": "https://example.com" }
            }
        }"#;
        let configs = parse_opencode_json(json).unwrap();
        match &configs["remote-tool"] {
            MCPServerConfig::Http { url, .. } => assert_eq!(url, "https://example.com"),
            other => panic!("expected Http config, got {other:?}"),
        }
    }

    #[test]
    fn parse_opencode_json_remote_sse_subtype() {
        let json = r#"{
            "mcp": {
                "remote-tool": { "type": "remote", "subtype": "sse", "url": "https://example.com" }
            }
        }"#;
        let configs = parse_opencode_json(json).unwrap();
        match &configs["remote-tool"] {
            MCPServerConfig::Sse { url, .. } => assert_eq!(url, "https://example.com"),
            other => panic!("expected Sse config, got {other:?}"),
        }
    }

    #[test]
    fn parse_opencode_json_skips_disabled_servers() {
        let json = r#"{
            "mcp": {
                "off-tool": { "type": "local", "command": ["x"], "enabled": false }
            }
        }"#;
        let configs = parse_opencode_json(json).unwrap();
        assert!(!configs.contains_key("off-tool"));
    }

    #[test]
    fn parse_opencode_json_unknown_type_skipped() {
        let json = r#"{
            "mcp": {
                "mystery": { "type": "future-transport" }
            }
        }"#;
        let configs = parse_opencode_json(json).unwrap();
        assert!(!configs.contains_key("mystery"));
    }
}
