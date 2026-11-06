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

/// Откуда читать чужие файлы пользователя; в тестах подменяется папками-заглушками.
pub(crate) struct Roots {
    pub home: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
}

impl Roots {
    pub fn system() -> Self {
        Self {
            home: dirs::home_dir(),
            config_dir: dirs::config_dir(),
        }
    }
}

/// Все восемь источников в порядке приоритета (первый, занявший имя, побеждает).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Own,
    Project,
    Global,
    Opencode,
    ClaudeCli,
    ClaudeDesktop,
    Vscode,
    Cursor,
}

/// Какие источники читать. Прогон с `--config`/`--data-dir` герметичен: файлы пользователя
/// (`~/.cursor`, Claude, VS Code, opencode…) не читаются; свой `mcp.json` — только из заданной
/// папки данных, а файлы проекта — только подтверждённые.
pub(crate) fn active_sources(isolated: bool, data_dir_overridden: bool) -> Vec<Source> {
    use Source::*;
    if isolated {
        let mut sources = Vec::new();
        if data_dir_overridden {
            sources.push(Own);
        }
        sources.push(Project);
        return sources;
    }
    vec![
        Own,
        Project,
        Global,
        Opencode,
        ClaudeCli,
        ClaudeDesktop,
        Vscode,
        Cursor,
    ]
}

pub fn load_mcp_config() -> HashMap<String, MCPServerConfig> {
    let sources = active_sources(
        crate::config::is_isolated(),
        crate::config::data_dir_is_overridden(),
    );
    load_sources(&sources, &Roots::system(), &super::project::trusted_texts())
}

pub(crate) fn load_sources(
    sources: &[Source],
    roots: &Roots,
    project: &[(PathBuf, String)],
) -> HashMap<String, MCPServerConfig> {
    let mut configs = HashMap::new();
    for source in sources {
        match source {
            Source::Own => load_own_config(&mut configs),
            Source::Project => load_project_config(project, &mut configs),
            Source::Global => load_global_config(roots, &mut configs),
            Source::Opencode => load_opencode_config(roots, &mut configs),
            Source::ClaudeCli => load_claude_cli_config(roots, &mut configs),
            Source::ClaudeDesktop => load_claude_desktop_config(roots, &mut configs),
            Source::Vscode => load_vscode_config(roots, &mut configs),
            Source::Cursor => load_cursor_config(roots, &mut configs),
        }
    }
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

/// Файлы проекта: только подтверждённые человеком, по тексту, с которого посчитан хэш.
fn load_project_config(
    project: &[(PathBuf, String)],
    configs: &mut HashMap<String, MCPServerConfig>,
) {
    for (_, text) in project {
        if let Ok(parsed) = parse_mcp_json(text) {
            merge_parsed(parsed, configs);
        }
    }
}

fn load_global_config(roots: &Roots, configs: &mut HashMap<String, MCPServerConfig>) {
    // Нет папки конфига — нет и файла: путь «от текущей папки» был бы чужим файлом проекта.
    let Some(dir) = &roots.config_dir else {
        return;
    };
    load_from_path(
        &dir.join("pooprusteek").join("mcp.config.json"),
        parse_mcp_json,
        configs,
    );
}

fn load_claude_desktop_config(roots: &Roots, configs: &mut HashMap<String, MCPServerConfig>) {
    let config_path = if cfg!(target_os = "macos") {
        roots
            .home
            .as_ref()
            .map(|h| h.join("Library/Application Support/Claude/claude_desktop_config.json"))
    } else {
        // Windows and Linux both keep it under the platform config dir.
        roots
            .config_dir
            .as_ref()
            .map(|c| c.join("Claude/claude_desktop_config.json"))
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

fn load_vscode_config(roots: &Roots, configs: &mut HashMap<String, MCPServerConfig>) {
    let settings_path = roots
        .config_dir
        .as_ref()
        .map(|c| c.join("Code/User/settings.json"));

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

fn load_cursor_config(roots: &Roots, configs: &mut HashMap<String, MCPServerConfig>) {
    let cursor_path = roots.home.as_ref().map(|h| h.join(".cursor/mcp.json"));

    if let Some(path) = cursor_path {
        load_from_path(&path, parse_mcp_json, configs);
    }
}

fn load_opencode_config(roots: &Roots, configs: &mut HashMap<String, MCPServerConfig>) {
    let base = roots
        .home
        .as_ref()
        .map(|h| h.join(".config").join("opencode"));
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

fn load_claude_cli_config(roots: &Roots, configs: &mut HashMap<String, MCPServerConfig>) {
    let candidates = [
        // ~/.claude/mcp.json (standard Claude CLI location)
        roots
            .home
            .as_ref()
            .map(|h| h.join(".claude").join("mcp.json")),
        // Also check XDG-like ~/.config/claude/mcp.json
        roots
            .config_dir
            .as_ref()
            .map(|c| c.join("claude").join("mcp.json")),
    ];

    for candidate in candidates.iter().flatten() {
        if load_from_path(candidate, parse_mcp_json, configs) {
            // take the first found file only
            return;
        }
    }
}

pub(crate) fn parse_mcp_json(
    content: &str,
) -> Result<HashMap<String, MCPServerConfig>, serde_json::Error> {
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

    fn user_files() -> (PathBuf, Roots) {
        let root = std::env::temp_dir().join(format!(
            "pooprusteek-mcproots-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("home");
        let config_dir = root.join("cfg");
        for (dir, name) in [
            (home.join(".cursor"), "mcp.json"),
            (home.join(".claude"), "mcp.json"),
            (config_dir.join("pooprusteek"), "mcp.config.json"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            let server = dir.file_name().unwrap().to_string_lossy().into_owned();
            std::fs::write(
                dir.join(name),
                format!(r#"{{"mcpServers":{{"user-{server}":{{"command":"node"}}}}}}"#),
            )
            .unwrap();
        }
        let roots = Roots {
            home: Some(home),
            config_dir: Some(config_dir),
        };
        (root, roots)
    }

    /// Прогон с `--config`/`--data-dir` не читает MCP-файлы пользователя; обычный запуск читает.
    #[test]
    fn an_isolated_run_reads_no_user_mcp_sources() {
        let (root, roots) = user_files();
        let user = [Source::Global, Source::ClaudeCli, Source::Cursor];

        let normal = load_sources(&user, &roots, &[]);
        assert_eq!(
            normal.len(),
            3,
            "an ordinary run sees the user's servers: {normal:?}"
        );

        for data_dir_overridden in [false, true] {
            let sources = active_sources(true, data_dir_overridden);
            assert!(
                sources
                    .iter()
                    .all(|s| matches!(s, Source::Own | Source::Project)),
                "{sources:?}"
            );
            // Own не читаем в тесте: это настоящий mcp.json процесса.
            let without_own: Vec<_> = sources.into_iter().filter(|s| *s != Source::Own).collect();
            assert!(load_sources(&without_own, &roots, &[]).is_empty());
        }
        // Свой `mcp.json` — только из заданной папки данных, не из настоящей.
        assert!(!active_sources(true, false).contains(&Source::Own));
        assert!(active_sources(true, true).contains(&Source::Own));
        assert_eq!(active_sources(false, false).len(), 8);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Без папки конфига глобальный источник не превращается в файл «от текущей папки».
    #[test]
    fn a_missing_config_dir_is_not_a_relative_path() {
        let roots = Roots {
            home: None,
            config_dir: None,
        };
        let sources = [
            Source::Global,
            Source::Cursor,
            Source::ClaudeCli,
            Source::Vscode,
        ];
        assert!(load_sources(&sources, &roots, &[]).is_empty());
    }

    /// Файлы проекта попадают в менеджер только как подтверждённые тексты.
    #[test]
    fn project_servers_come_only_from_confirmed_texts() {
        let roots = Roots {
            home: None,
            config_dir: None,
        };
        assert!(load_sources(&[Source::Project], &roots, &[]).is_empty());
        let confirmed = vec![(
            PathBuf::from(".vscode/mcp.json"),
            r#"{"servers":{"ok":{"command":"node"}}}"#.to_string(),
        )];
        let loaded = load_sources(&[Source::Project], &roots, &confirmed);
        assert!(loaded.contains_key("ok"));
    }
}
