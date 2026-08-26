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
    #[expect(
        dead_code,
        reason = "no intermediate progress point in the current connect path"
    )]
    Connecting,
    Connected,
    /// The server answered with HTTP 401 during connect. The `Option<String>`
    /// is a discovery hint — the `resource_metadata` URL pulled out of a
    /// `WWW-Authenticate` header (RFC 9728), when the server sent one — used
    /// by `mcp::oauth::discover` to skip straight to the right metadata URL
    /// instead of guessing the well-known path.
    AuthRequired(Option<String>),
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

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default)]
pub struct McpViewState {
    pub active: bool,
    /// Курсор по `visible_indices()`. Окно выводится из него на отрисовке
    /// (`ListWindow::anchored`): высота этой панели известна только там.
    pub selected: usize,
    /// Скролл текста в панели деталей — не списка. Раньше это было одно поле
    /// на двоих, и прокрутка деталей оставляла окно списка на чужом смещении.
    pub details_scroll: usize,
    pub details_server: Option<String>,
    pub servers: Vec<ServerDisplayInfo>,
    pub status_message: String,
    /// Set by `/mcp auth` (or `/mcp oauth`) — while true, the list only shows
    /// (and Up/Down/Enter only navigate) servers with `needs_auth`, and Enter
    /// starts the OAuth flow instead of opening the details panel. `servers`
    /// itself stays the full, unfiltered list — it's kept in sync by the
    /// throttled `McpStatus::update_stats` poll (`app/mcp_status.rs`), which
    /// would silently undo any in-place filtering within ~2s.
    pub auth_mode: bool,
}

impl McpViewState {
    /// Indices into `servers` that should be shown/navigable right now: every
    /// index in manage mode, or only `needs_auth` ones in `auth_mode`.
    pub fn visible_indices(&self) -> Vec<usize> {
        (0..self.servers.len())
            .filter(|&i| !self.auth_mode || self.servers[i].needs_auth)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ServerDisplayInfo {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub tool_count: usize,
    pub resource_count: usize,
    pub enabled: bool,
    /// True when `status` is `MCPServerStatus::AuthRequired(_)`. The
    /// discovery hint itself isn't carried here — `/mcp auth`'s Enter
    /// handler re-reads it live from `MCPManager::oauth_context` when it
    /// actually starts a flow, rather than snapshotting it into this
    /// display struct.
    pub needs_auth: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, needs_auth: bool) -> ServerDisplayInfo {
        ServerDisplayInfo {
            name: name.to_string(),
            transport: "http".to_string(),
            status: if needs_auth {
                "auth required".to_string()
            } else {
                "connected".to_string()
            },
            tool_count: 0,
            resource_count: 0,
            enabled: true,
            needs_auth,
        }
    }

    #[test]
    fn visible_indices_manage_mode_shows_everything() {
        let view = McpViewState {
            servers: vec![server("a", false), server("b", true), server("c", false)],
            auth_mode: false,
            ..Default::default()
        };
        assert_eq!(view.visible_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn visible_indices_auth_mode_filters_to_needs_auth_only() {
        let view = McpViewState {
            servers: vec![server("a", false), server("b", true), server("c", true)],
            auth_mode: true,
            ..Default::default()
        };
        assert_eq!(view.visible_indices(), vec![1, 2]);
    }

    #[test]
    fn visible_indices_auth_mode_empty_when_nothing_needs_auth() {
        let view = McpViewState {
            servers: vec![server("a", false), server("b", false)],
            auth_mode: true,
            ..Default::default()
        };
        assert!(view.visible_indices().is_empty());
    }
}
