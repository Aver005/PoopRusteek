//! Shared value types for the MCP subsystem: server config/status,
//! discovered tools/resources, and the UI-facing display structs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport")]
pub enum MCPServerConfig {
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
        #[serde(default)]
        cwd: Option<String>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "sse")]
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MCPServerStatus {
    Pending,
    // The connect path resolves straight to `Connected`/`Error` in one
    // await with no intermediate progress point, so nothing constructs this
    // today. The status-label match in manager.rs already handles it, so
    // it's kept rather than removed.
    #[expect(dead_code, reason = "no intermediate progress point in the current connect path")]
    Connecting,
    Connected,
    Error(String),
    Disabled,
}

/// Where a server's config came from — determines what `MCPManager`'s
/// `persist_config` is allowed to write for it. `Own` servers were found in
/// our own `mcp.json` (or added/edited in-app in the future); their full
/// config is ours to freely rewrite. `Foreign` servers were discovered from
/// another tool's config (Claude Desktop, VS Code, Cursor, opencode, the
/// Claude CLI, or a workspace file) — read-only sources we must never copy
/// wholesale into our own file, since their `env` maps can carry secrets
/// that belong to that other tool, not to us. Only a `Foreign` server's
/// enabled/disabled state (as a name-keyed override, no config) is ever
/// persisted — see `config::save_mcp_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSource {
    Own,
    Foreign,
}

#[derive(Debug, Clone)]
pub struct MCPTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub server_name: String,
}

#[derive(Debug, Clone)]
pub struct MCPResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    #[serde(default)]
    pub resources: Option<serde_json::Value>,
    #[serde(default)]
    pub prompts: Option<serde_json::Value>,
    #[serde(default)]
    pub logging: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct MCPToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(flatten)]
    pub config: MCPServerConfig,
}

fn default_enabled() -> bool { true }

#[derive(Debug, Clone, Default)]
pub struct McpViewState {
    pub active: bool,
    pub selected: usize,
    pub scroll_offset: usize,
    pub details_server: Option<String>,
    pub servers: Vec<ServerDisplayInfo>,
    pub status_message: String,
}

#[derive(Debug, Clone)]
pub struct ServerDisplayInfo {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub tool_count: usize,
    pub resource_count: usize,
    pub enabled: bool,
}
