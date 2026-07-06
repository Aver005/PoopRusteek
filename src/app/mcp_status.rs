//! MCP status shown in the UI.
//!
//! Distinct from `App.mcp` (the live [`MCPManager`](crate::mcp::MCPManager)):
//! this is the *view* of MCP that the TUI renders — the panel state plus the
//! server counts and a refresh-throttle timestamp — previously four loose
//! fields on the `AppState` god-object. The counts are mirrored here from the
//! manager on a throttled poll so rendering never has to take the manager lock.

use crate::mcp::MCPManager;
use crate::mcp::types::McpViewState;
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
    /// Returns whether the visible numbers changed (drives the dirty flag).
    ///
    /// `try_lock`, never `lock`: this runs on the event loop, and awaiting a
    /// manager mutex that a slow operation holds would freeze the whole UI.
    /// Stale counts for a poll interval are invisible; a frozen TUI is not.
    pub async fn update_stats(&mut self, mcp: &Mutex<MCPManager>) -> bool {
        if let Some(last) = self.last_stats_update
            && last.elapsed().as_secs() < Self::STATS_INTERVAL_SECS
        {
            return false;
        }
        let Ok(manager) = mcp.try_lock() else {
            return false;
        };
        let servers = manager.get_servers_info();
        drop(manager);

        let connected = servers
            .iter()
            .filter(|s| s.enabled && s.status == "connected")
            .count();
        let changed = connected != self.connected_count || servers.len() != self.server_count;
        self.connected_count = connected;
        self.server_count = servers.len();
        self.view.servers = servers;
        self.last_stats_update = Some(Instant::now());
        changed
    }

    /// Lazily populate the detail panel's server list when it is first shown.
    /// Returns whether the list was (re)populated.
    pub async fn refresh_view(&mut self, mcp: &Mutex<MCPManager>) -> bool {
        if !self.view.servers.is_empty() {
            return false;
        }
        let Ok(manager) = mcp.try_lock() else {
            return false;
        };
        self.view.servers = manager.get_servers_info();
        !self.view.servers.is_empty()
    }
}
