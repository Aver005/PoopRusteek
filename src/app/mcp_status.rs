//! MCP status shown in the UI.
//!
//! Distinct from `App.mcp` (the live [`MCPManager`](crate::mcp::MCPManager)):
//! this is the *view* of MCP that the TUI renders — the panel state plus the
//! server counts and a refresh-throttle timestamp — previously four loose
//! fields on the `AppState` god-object. The counts are mirrored here from the
//! manager on a throttled poll so rendering never has to take the manager lock.

use crate::mcp::types::McpViewState;
use crate::mcp::MCPManager;
use std::time::Instant;
use tokio::sync::Mutex;

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

impl McpStatus {
    /// Minimum gap between throttled count refreshes.
    const STATS_INTERVAL_SECS: u64 = 2;

    /// Throttled refresh of the cached counts and server list from the manager.
    pub async fn update_stats(&mut self, mcp: &Mutex<MCPManager>) {
        if let Some(last) = self.last_stats_update {
            if last.elapsed().as_secs() < Self::STATS_INTERVAL_SECS {
                return;
            }
        }
        let servers = mcp.lock().await.get_servers_info();
        self.connected_count = servers
            .iter()
            .filter(|s| s.enabled && s.status == "connected")
            .count();
        self.server_count = servers.len();
        self.view.servers = servers;
        self.last_stats_update = Some(Instant::now());
    }

    /// Lazily populate the detail panel's server list when it is first shown.
    pub async fn refresh_view(&mut self, mcp: &Mutex<MCPManager>) {
        if !self.view.servers.is_empty() {
            return;
        }
        self.view.servers = mcp.lock().await.get_servers_info();
    }
}
