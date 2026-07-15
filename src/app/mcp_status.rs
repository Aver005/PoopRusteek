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

impl crate::app::App {
    /// `AppEvent::McpOperationDone` — announce the outcome, force the next
    /// loop iteration to re-pull fresh server info, and re-embed the MCP
    /// corpus (add/reload/toggle/reconnect/remove all funnel through this).
    pub(crate) fn on_mcp_operation_done(&mut self, message: String) {
        self.state.mcp_status.view.status_message = message.clone();
        self.announce(message);
        self.state.mcp_status.view.servers.clear();
        self.state.mcp_status.last_stats_update = None;
        self.spawn_mcp_semantic_refresh();
    }

    /// `AppEvent::McpInitialized` — startup connects finished; clear the
    /// cached view and skip the stats-poll throttle so fresh counts appear
    /// immediately. (The startup task already updated the semantic corpus.)
    pub(crate) fn on_mcp_initialized(&mut self) {
        self.state.mcp_status.view.servers.clear();
        self.state.mcp_status.last_stats_update = None;
    }

    /// `AppEvent::McpOAuthResult` — on success, reconnect the server off the
    /// event loop; the token is already persisted (oauth_store::save), so
    /// `build_client` picks it up, and the reconnect reports its own outcome
    /// through the existing `McpOperationDone` funnel.
    pub(crate) fn on_mcp_oauth_result(&mut self, server: String, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.state.mcp_status.view.status_message =
                    format!("{server} authorized, reconnecting...");
                let mcp = std::sync::Arc::clone(&self.mcp);
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let message = match mcp.lock().await.reconnect_server(&server).await {
                        Err(e) => format!("Reconnect after authorization failed: {e}"),
                        Ok(_) => format!("{server} authorized and reconnected"),
                    };
                    let _ =
                        event_tx.send(crate::app::events::AppEvent::McpOperationDone { message });
                });
            }
            Err(e) => {
                self.state.mcp_status.view.status_message = format!("Authorization failed: {e}");
            }
        }
    }

    /// Fetch the current MCP tool list under a short-lived lock off the
    /// event loop and hand it to the semantic layer for re-embedding — one
    /// definition for the sites that used to copy this block (the startup
    /// wiring keeps its own inline copy: it also signals `McpInitialized`).
    pub(crate) fn spawn_mcp_semantic_refresh(&self) {
        let mcp = std::sync::Arc::clone(&self.mcp);
        let semantic = std::sync::Arc::clone(&self.semantic);
        tokio::spawn(async move {
            let tools = mcp.lock().await.get_all_tools();
            semantic.update_mcp_tools(tools);
        });
    }
}
