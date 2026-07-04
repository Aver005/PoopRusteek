pub mod jsonrpc;
pub mod transport;
pub mod client;
pub mod manager;
pub mod config;
pub mod types;
pub mod oauth;
pub mod oauth_store;

pub use manager::MCPManager;

/// Prefix that turns a server's tool into its registry-wide name:
/// `mcp__<server>__<tool>`. `MCPManager` owns both sides (building the full
/// names and resolving them back via `tool_name_map`); everyone else —
/// agent dispatch, whitelisting — only ever checks `starts_with` on this
/// constant.
pub const MCP_TOOL_PREFIX: &str = "mcp__";
