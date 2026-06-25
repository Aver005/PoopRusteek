use super::types::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct ClaudeDesktopConfig {
    #[serde(default)]
    mcpServers: HashMap<String, ClaudeDesktopServer>,
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
struct VSCodeMCPConfig {
    #[serde(default)]
    servers: HashMap<String, VSCodeMCPServer>,
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

    load_workspace_config(&mut configs);
    load_global_config(&mut configs);
    load_opencode_config(&mut configs);
    load_claude_cli_config(&mut configs);
    load_claude_desktop_config(&mut configs);
    load_vscode_config(&mut configs);
    load_cursor_config(&mut configs);

    configs
}

fn load_workspace_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let paths = vec![
        PathBuf::from("mcp.config.json"),
        PathBuf::from(".vscode/mcp.json"),
    ];

    for path in paths {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(mut workspace_configs) = parse_mcp_json(&content) {
                    for (name, config) in workspace_configs.drain() {
                        configs.entry(name).or_insert(config);
                    }
                }
            }
        }
    }
}

fn load_global_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pooprusteek")
        .join("mcp.config.json");

    if config_dir.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_dir) {
            if let Ok(mut global_configs) = parse_mcp_json(&content) {
                for (name, config) in global_configs.drain() {
                    configs.entry(name).or_insert(config);
                }
            }
        }
    }
}

fn load_claude_desktop_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let config_path = if cfg!(target_os = "macos") {
        dirs::home_dir()
            .map(|h| h.join("Library/Application Support/Claude/claude_desktop_config.json"))
    } else if cfg!(target_os = "windows") {
        dirs::config_dir()
            .map(|c| c.join("Claude/claude_desktop_config.json"))
    } else {
        dirs::config_dir()
            .map(|c| c.join("Claude/claude_desktop_config.json"))
    };

    if let Some(path) = config_path {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(desktop_config) = serde_json::from_str::<ClaudeDesktopConfig>(&content) {
                    for (name, server) in desktop_config.mcpServers {
                        let config = MCPServerConfig::Stdio {
                            command: server.command,
                            args: server.args,
                            env: server.env,
                            cwd: None,
                        };
                        configs.entry(name).or_insert(config);
                    }
                }
            }
        }
    }
}

fn load_vscode_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let settings_path = dirs::config_dir()
        .map(|c| c.join("Code/User/settings.json"));

    if let Some(path) = settings_path {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(settings) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(mcp) = settings.get("mcp") {
                        if let Some(servers) = mcp.get("servers") {
                            if let Some(obj) = servers.as_object() {
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
                                        configs.entry(name.clone()).or_insert(config);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn load_cursor_config(configs: &mut HashMap<String, MCPServerConfig>) {
    let cursor_path = dirs::home_dir()
        .map(|h| h.join(".cursor/mcp.json"));

    if let Some(path) = cursor_path {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(mut cursor_configs) = parse_mcp_json(&content) {
                    for (name, config) in cursor_configs.drain() {
                        configs.entry(name).or_insert(config);
                    }
                }
            }
        }
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

    for path in &candidates {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                // try opencode format first, fall back to standard mcp format
                let is_opencode = path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("opencode"))
                    .unwrap_or(false);

                if is_opencode {
                    if let Ok(mut parsed) = parse_opencode_json(&content) {
                        for (name, config) in parsed.drain() {
                            configs.entry(name).or_insert(config);
                        }
                        return;
                    }
                    // fall through to standard parser if opencode format fails
                }

                if let Ok(mut parsed) = parse_mcp_json(&content) {
                    for (name, config) in parsed.drain() {
                        configs.entry(name).or_insert(config);
                    }
                }
            }
            return;
        }
    }
}

fn parse_opencode_json(content: &str) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(content)?;
    let mut configs = HashMap::new();

    if let Some(mcp) = value.get("mcp") {
        if let Some(obj) = mcp.as_object() {
            for (name, server) in obj {
                // skip disabled servers
                if server.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                    continue;
                }

                let config = match server.get("type").and_then(|t| t.as_str()) {
                    Some("remote") => {
                        let url = server.get("url")
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string();
                        let headers = server.get("headers")
                            .and_then(|h| serde_json::from_value(h.clone()).ok())
                            .unwrap_or_default();
                        MCPServerConfig::Http { url, headers }
                    }
                    Some("local") => {
                        let (command, args) = if let Some(cmd_arr) = server.get("command").and_then(|c| c.as_array()) {
                            let mut parts: Vec<String> = cmd_arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect();
                            let cmd = if parts.is_empty() { String::new() } else { parts.remove(0) };
                            (cmd, parts)
                        } else {
                            (String::new(), vec![])
                        };
                        let env = server.get("env")
                            .and_then(|e| serde_json::from_value(e.clone()).ok());
                        MCPServerConfig::Stdio { command, args, env, cwd: None }
                    }
                    _ => continue,
                };
                configs.insert(name.clone(), config);
            }
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
        if candidate.exists() {
            if let Ok(content) = std::fs::read_to_string(candidate) {
                if let Ok(mut claude_configs) = parse_mcp_json(&content) {
                    for (name, config) in claude_configs.drain() {
                        configs.entry(name).or_insert(config);
                    }
                }
            }
            // take the first found file only
            return;
        }
    }
}

fn parse_mcp_json(content: &str) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(content)?;
    let mut configs = HashMap::new();

    if let Some(servers) = value.get("mcpServers").or_else(|| value.get("servers")) {
        if let Some(obj) = servers.as_object() {
            for (name, server) in obj {
                let config = if let Some(url) = server.get("url").and_then(|u| u.as_str()) {
                    let headers = server.get("headers")
                        .and_then(|h| serde_json::from_value(h.clone()).ok())
                        .unwrap_or_default();
                    MCPServerConfig::Http {
                        url: url.to_string(),
                        headers,
                    }
                } else {
                    let command = server.get("command")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = server.get("args")
                        .and_then(|a| serde_json::from_value(a.clone()).ok())
                        .unwrap_or_default();
                    let env = server.get("env")
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
    }

    Ok(configs)
}
