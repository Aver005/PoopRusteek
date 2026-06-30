//! MCP status shown in the UI.
//!
//! Distinct from `App.mcp` (the live [`MCPManager`](crate::mcp::MCPManager)):
//! this is the *view* of MCP that the TUI renders — the panel state plus the
//! server counts and a refresh-throttle timestamp — previously four loose
//! fields on the `AppState` god-object. The counts are mirrored here from the
//! manager on a throttled poll so rendering never has to take the manager lock.

use crate::mcp::types::McpViewState;
use std::time::Instant;

/// UI-facing MCP status: panel view + cached server counts.
#[derive(Default)]
pub struct McpStatus {
    /// State of the MCP detail panel (selection, scroll, server list).
    pub view: McpViewState,
    /// Total configured servers, mirrored from the manager.
    pub server_count: usize,
    /// Servers currently connected, mirrored from the manager.
    pub connected_count: usize,
    /// When the counts were last refreshed, for poll throttling.
    pub last_stats_update: Option<Instant>,
}
