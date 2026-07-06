//! Owns every configured MCP server: connecting, reconnecting, caching tool
//! lists, routing tool calls, and persisting user overrides.
//!
//! Two structural things worth knowing before reading the rest of this file:
//!
//! - Every server's client is `Arc<MCPClient>`. That's what lets
//!   `client_for` hand a caller (the agent loop) a handle it can run a tool
//!   call through *without* holding `MCPManager`'s own lock — previously the
//!   whole `Mutex<MCPManager>` was held across the network await inside
//!   `call_tool`, which froze the UI event loop for the duration of every
//!   MCP tool call. `call_tool` on this type still exists and still works
//!   (it just delegates to `client_for` internally) so existing callers
//!   don't need to change.
//! - Each entry remembers a [`ServerSource`]: `Own` (from our `mcp.json`) or
//!   `Foreign` (discovered from another tool's config). `persist_config`
//!   uses this to avoid writing foreign servers' full config — including
//!   their `env` secrets — into our own file; see its doc comment.

use super::client::MCPClient;
use super::config::{load_mcp_config, load_overrides, load_own_enabled_map, save_mcp_config};
use super::types::*;
use crate::error::AppResult;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Determine a discovered server's enabled state and [`ServerSource`].
///
/// `own_enabled` is the key set of our own `mcpServers` section
/// (`load_own_enabled_map`) — a name present there is `ServerSource::Own`,
/// full stop, regardless of what other tool might coincidentally also
/// define a server with that name (our own config always wins, matching
/// `load_mcp_config`'s discovery order). Everything else is
/// `ServerSource::Foreign`; its enabled state comes from `foreign_overrides`
/// (`load_overrides`) if the user has toggled it in-app before, defaulting
/// to enabled otherwise (matches prior behavior, where every discovered
/// server defaulted to enabled).
fn resolve_enabled_and_source(
    name: &str,
    own_enabled: &HashMap<String, bool>,
    foreign_overrides: &HashMap<String, bool>,
) -> (bool, ServerSource) {
    if let Some(&enabled) = own_enabled.get(name) {
        (enabled, ServerSource::Own)
    } else {
        let enabled = foreign_overrides.get(name).copied().unwrap_or(true);
        (enabled, ServerSource::Foreign)
    }
}

struct ServerCache {
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    tool_entries: Vec<(String, String, String)>,  // (full_name, server_name, tool_name)
    cached_at: Instant,
}

pub struct MCPManager {
    servers: HashMap<String, MCPServerEntry>,
    tool_name_map: HashMap<String, (String, String)>,
    cache: HashMap<String, ServerCache>,
    cache_ttl: u64,
}

struct MCPServerEntry {
    client: Arc<MCPClient>,
    config: MCPServerConfig,
    status: MCPServerStatus,
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    enabled: bool,
    source: ServerSource,
}

/// A read-only snapshot of a server's `ServerCache` entry, cloned out before
/// fanning connections out concurrently (see `connect_all`) so `connect_one`
/// never needs to borrow `MCPManager` itself.
struct CachedSnapshot {
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    tool_entries: Vec<(String, String, String)>,
    age_secs: u64,
}

/// Result of connecting (or failing to connect) one server, produced by
/// `connect_one` and merged back into `MCPManager` state by `connect_all`.
struct ConnectOutcome {
    name: String,
    status: MCPServerStatus,
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    tool_entries: Vec<(String, String, String)>,
    /// True if `tools`/`resources` came from `cached` rather than a fresh
    /// `list_tools`/`list_resources` call — tells `connect_all` whether it
    /// still needs to insert a (fresh) `ServerCache` entry.
    used_cache: bool,
}

/// Initialize + handshake + (list-tools-or-use-cache) for one server. A free
/// function (not an `MCPManager` method) so `connect_all` can run many of
/// these concurrently via `futures::future::join_all` — see its doc comment
/// for why that requires *not* taking `&mut self` here.
async fn connect_one(
    name: String,
    client: Arc<MCPClient>,
    cached: Option<CachedSnapshot>,
    cache_ttl: u64,
) -> ConnectOutcome {
    let empty = |status: MCPServerStatus| ConnectOutcome {
        name: name.clone(),
        status,
        tools: Vec::new(),
        resources: Vec::new(),
        tool_entries: Vec::new(),
        used_cache: false,
    };

    if let Err(e) = client.initialize().await {
        if let crate::error::AppError::McpAuthRequired { www_authenticate } = &e {
            let hint = www_authenticate.as_deref().and_then(super::oauth::extract_resource_metadata);
            tracing::info!("MCP '{name}' requires authorization");
            return empty(MCPServerStatus::AuthRequired(hint));
        }
        tracing::warn!("MCP '{name}' initialize failed: {e}");
        return empty(MCPServerStatus::Error(e.to_string()));
    }
    if let Err(e) = client.send_initialized().await {
        tracing::warn!("MCP '{name}' initialized but send_initialized failed: {e}");
        return empty(MCPServerStatus::Error(e.to_string()));
    }

    if let Some(cache) = cached {
        if cache.age_secs < cache_ttl {
            tracing::info!("MCP '{name}' using cache ({}s old, TTL {cache_ttl})", cache.age_secs);
            return ConnectOutcome {
                name,
                status: MCPServerStatus::Connected,
                tools: cache.tools,
                resources: cache.resources,
                tool_entries: cache.tool_entries,
                used_cache: true,
            };
        }
        tracing::info!("MCP '{name}' cache expired ({}s >= TTL {cache_ttl}s)", cache.age_secs);
    }

    // Fetch from server. A failed `list_tools` used to still fall through to
    // `Connected` below — silently reporting a broken server as healthy with
    // zero tools. Treat it as a connection failure instead; `list_resources`
    // is best-effort/optional either way (many servers don't implement it).
    let tools = match client.list_tools().await {
        Ok(tools) => tools,
        Err(e) => {
            tracing::warn!("MCP '{name}' list_tools failed: {e}");
            return empty(MCPServerStatus::Error(format!("list_tools failed: {e}")));
        }
    };

    let mut tool_entries = Vec::new();
    for tool in &tools {
        let full_name = format!("{}{}__{}", crate::mcp::MCP_TOOL_PREFIX, name, tool.name);
        tool_entries.push((full_name, name.clone(), tool.name.clone()));
    }

    let resources = match client.list_resources().await {
        Ok(r) => {
            tracing::info!("MCP '{name}' fetched {} resources", r.len());
            r
        }
        Err(e) => {
            tracing::info!("MCP '{name}' list_resources (optional): {e}");
            Vec::new()
        }
    };

    tracing::info!("MCP '{name}' connected with {} tools", tools.len());
    ConnectOutcome {
        name,
        status: MCPServerStatus::Connected,
        tools,
        resources,
        tool_entries,
        used_cache: false,
    }
}

impl MCPManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            tool_name_map: HashMap::new(),
            cache: HashMap::new(),
            cache_ttl: 300,
        }
    }

    pub fn set_cache_ttl(&mut self, ttl: u64) {
        self.cache_ttl = ttl;
    }

    /// Re-discover and reconnect every server from disk.
    ///
    /// Every existing client is explicitly `close()`d before being dropped
    /// (child process killed for stdio; no-op for HTTP/SSE) — previously
    /// `self.servers.clear()` just dropped every `MCPServerEntry`, and
    /// nothing had ever called `Transport::close()`, so every stdio child
    /// process leaked as an orphan on each reload.
    pub async fn reload_all(&mut self) {
        self.cache.clear();
        self.tool_name_map.clear();
        self.close_all_clients().await;
        self.servers.clear();

        let configs = load_mcp_config();
        let own_enabled = load_own_enabled_map();
        let foreign_overrides = load_overrides();

        for (name, config) in configs {
            let (enabled, source) = resolve_enabled_and_source(&name, &own_enabled, &foreign_overrides);
            self.add_server(name, config, enabled, source).await;
        }

        self.connect_all().await;
    }

    /// Close every server's transport (kills stdio child processes) without
    /// touching `self.servers` itself. Shared by `reload_all` (which then
    /// clears the map) and `shutdown_all` (which the app should call on
    /// exit — see that method's doc comment).
    async fn close_all_clients(&self) {
        for (name, entry) in &self.servers {
            if let Err(e) = entry.client.close().await {
                tracing::debug!("MCP '{name}' close failed (best-effort): {e}");
            }
        }
    }

    /// Close every managed server's transport. Not wired into any
    /// app-lifecycle hook by this change — the app's exit path should call
    /// this before dropping its `Arc<Mutex<MCPManager>>` so stdio child
    /// processes don't leak as orphans when the app quits normally. See the
    /// final summary for exactly where that call belongs.
    pub async fn shutdown_all(&mut self) {
        self.close_all_clients().await;
    }

    async fn add_server(&mut self, name: String, config: MCPServerConfig, enabled: bool, source: ServerSource) {
        if !enabled {
            let entry = MCPServerEntry {
                client: Arc::new(MCPClient::dummy(&name)),
                config,
                status: MCPServerStatus::Disabled,
                tools: Vec::new(),
                resources: Vec::new(),
                enabled: false,
                source,
            };
            self.servers.insert(name, entry);
            return;
        }
        let client = match build_client(&name, &config).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("Failed to create MCP client for '{name}': {e}");
                None
            }
        };

        if let Some(client) = client {
            self.servers.insert(name, MCPServerEntry {
                client: Arc::new(client),
                config,
                status: MCPServerStatus::Pending,
                tools: Vec::new(),
                resources: Vec::new(),
                enabled: true,
                source,
            });
        }
    }

    /// Connect every server concurrently.
    ///
    /// Previously this awaited `connect_server` one server at a time, so N
    /// slow servers meant roughly N times the wait before *any* of them was
    /// usable — a single slow-to-start server head-of-line-blocked every
    /// other one. `connect_one` (a free function, not a method) does the
    /// actual work against just an `Arc<MCPClient>` and a cache snapshot —
    /// deliberately not against `&mut self` — so `join_all` can run all of
    /// them at once without needing N simultaneous mutable borrows of
    /// `self` (which isn't possible in safe Rust). Each server's outcome is
    /// applied back into `self.servers`/`self.tool_name_map`/`self.cache`
    /// afterward, in a plain non-async loop — cheap, and avoids any
    /// interleaving of the merge step itself. One server's connection
    /// failure can't affect another's: each `connect_one` call is
    /// independently awaited and independently caught inside itself (see
    /// its own error handling), so a panic-free failure in one is just a
    /// `ConnectOutcome` with an `Error` status, not a short-circuit of the
    /// whole `join_all`.
    async fn connect_all(&mut self) {
        // Collect owned inputs for every server *before* awaiting anything —
        // `connect_one` takes only owned values (`Arc<MCPClient>` clone +
        // an owned `CachedSnapshot`), so this loop only ever holds a shared
        // borrow of `self`, and that borrow is fully released by the time
        // `.collect()` returns, well before `join_all` runs or
        // `apply_connect_outcome` needs `&mut self`.
        let inputs: Vec<(String, Arc<MCPClient>, Option<CachedSnapshot>)> = self.servers.iter()
            .map(|(name, entry)| (name.clone(), Arc::clone(&entry.client), self.cache_snapshot(name)))
            .collect();
        let cache_ttl = self.cache_ttl;

        let jobs = inputs.into_iter()
            .map(|(name, client, cached)| connect_one(name, client, cached, cache_ttl));
        let outcomes = futures::future::join_all(jobs).await;

        for outcome in outcomes {
            self.apply_connect_outcome(outcome);
        }
    }

    /// Connect a single server by name — used by `toggle_server` and
    /// `reconnect_server`, which each need to (re)connect just the one
    /// server they're acting on rather than every server via `connect_all`.
    async fn connect_server(&mut self, name: &str) {
        let Some(entry) = self.servers.get(name) else { return };
        let client = Arc::clone(&entry.client);
        let cached = self.cache_snapshot(name);
        let outcome = connect_one(name.to_string(), client, cached, self.cache_ttl).await;
        self.apply_connect_outcome(outcome);
    }

    /// Clone a read-only snapshot of `name`'s cache entry, if any and not
    /// yet checked for TTL — the age is computed here so the snapshot is
    /// self-contained (no `Instant` comparison needed later).
    fn cache_snapshot(&self, name: &str) -> Option<CachedSnapshot> {
        self.cache.get(name).map(|c| CachedSnapshot {
            tools: c.tools.clone(),
            resources: c.resources.clone(),
            tool_entries: c.tool_entries.clone(),
            age_secs: c.cached_at.elapsed().as_secs(),
        })
    }

    /// Merge one `connect_one` result back into manager state: tool name
    /// map, tool-list cache (only for a freshly-fetched, non-cached,
    /// successful connect), and the server's own status/tools/resources.
    fn apply_connect_outcome(&mut self, outcome: ConnectOutcome) {
        // The server can vanish or be disabled between a lock-free connect
        // and this merge (`startup_initialize` phases 3→4 race a user's
        // /mcp remove or toggle) — applying a stale outcome would resurrect
        // it. Callers that connect under one continuous lock hold
        // (`connect_all`, `connect_server`) can't hit either branch.
        match self.servers.get(&outcome.name) {
            Some(entry) if entry.enabled => {}
            _ => return,
        }
        // A cache-served outcome and a fresh successful connect both carry
        // usable tool entries; a failed connect maps nothing.
        if outcome.used_cache || matches!(outcome.status, MCPServerStatus::Connected) {
            for (full_name, server_name, tool_name) in &outcome.tool_entries {
                self.tool_name_map.insert(full_name.clone(), (server_name.clone(), tool_name.clone()));
            }
            // Only a fresh fetch (re)fills the cache — see doc comment above.
            if !outcome.used_cache {
                self.cache.insert(outcome.name.clone(), ServerCache {
                    tools: outcome.tools.clone(),
                    resources: outcome.resources.clone(),
                    tool_entries: outcome.tool_entries.clone(),
                    cached_at: Instant::now(),
                });
            }
        }

        if let Some(entry) = self.servers.get_mut(&outcome.name) {
            entry.status = outcome.status;
            entry.tools = outcome.tools;
            entry.resources = outcome.resources;
        }
    }

    /// Look up the client and bare tool name for a `mcp__<server>__<tool>`
    /// prefixed tool name, without calling anything.
    ///
    /// This is what lets a caller (the agent loop) run a tool call without
    /// holding `MCPManager`'s own lock across the network await: clone the
    /// `Arc<MCPClient>` out here (cheap — bumps a refcount), drop the
    /// manager lock, then call `client.call_tool(...)` on the owned handle.
    /// Previously the *only* way to call a tool was `MCPManager::call_tool`,
    /// which — because it needs `&mut self` for the status check/flip
    /// below — could only be called while holding the manager's mutex for
    /// the whole round-trip, freezing the UI event loop for the duration of
    /// every MCP tool call.
    ///
    /// Returns `None` if the tool name is unknown or its server isn't
    /// currently `Connected` (mirrors the guard `call_tool` used to run
    /// inline). Reuses the same prefix parsing as `connect_one`'s
    /// `tool_name_map` population — the map itself, rather than
    /// re-splitting the `mcp__` string here — so there's exactly one place
    /// that understands the prefix scheme.
    pub fn client_for(&self, full_tool_name: &str) -> Option<(Arc<MCPClient>, String)> {
        let (server_name, tool_name) = self.tool_name_map.get(full_tool_name)?;
        let entry = self.servers.get(server_name)?;
        if !matches!(entry.status, MCPServerStatus::Connected) {
            return None;
        }
        Some((Arc::clone(&entry.client), tool_name.clone()))
    }

    /// Still `&mut self` and still does the full call inline — kept for
    /// existing callers (delegates to `client_for` for the lookup). New
    /// callers that want to avoid holding the manager lock across the
    /// network await should use `client_for` directly instead.
    ///
    /// A transport-level failure (`Err` from `MCPClient::call_tool` — I/O
    /// error, timeout, decode error) means the *connection* is unhealthy,
    /// so this flips the server to `MCPServerStatus::Error`. A tool that
    /// merely reported failure (`Ok(MCPToolResult { is_error: true, .. })`
    /// — a JSON-RPC error response or `isError: true`) does not; the
    /// connection is fine, the tool just failed, which is a normal outcome
    /// the caller/LLM handles on its own. Previously nothing ever flipped
    /// status back to `Error` after a successful connect, so a server whose
    /// process had died mid-session (or whose transport started timing
    /// out) would keep reporting "connected" indefinitely.
    ///
    /// The in-tree agent loop has since migrated to `client_for` (avoids
    /// holding the manager lock across the network await), so nothing here
    /// calls this anymore — kept anyway per the doc comment above.
    #[expect(dead_code, reason = "back-compat entry point, deliberately kept per doc comment above")]
    pub async fn call_tool(&mut self, full_tool_name: &str, args: Value) -> AppResult<MCPToolResult> {
        let server_name = self.tool_name_map.get(full_tool_name)
            .map(|(s, _)| s.clone())
            .ok_or_else(|| crate::error::AppError::Mcp(format!("Unknown MCP tool: {full_tool_name}")))?;

        let Some((client, tool_name)) = self.client_for(full_tool_name) else {
            // Either unknown (already ruled out above) or not connected.
            return Ok(MCPToolResult {
                content: format!("MCP server '{server_name}' is not connected"),
                is_error: true,
            });
        };

        match client.call_tool(&tool_name, args).await {
            Ok(result) => Ok(result),
            Err(e) => {
                if let Some(entry) = self.servers.get_mut(&server_name) {
                    entry.status = MCPServerStatus::Error(e.to_string());
                }
                tracing::warn!("MCP '{server_name}' tool call transport error, marking Error: {e}");
                Err(e)
            }
        }
    }

    pub fn get_all_resources(&self) -> Vec<MCPResource> {
        let mut result = Vec::new();
        for entry in self.servers.values() {
            result.extend(entry.resources.clone());
        }
        result
    }

    pub fn get_all_tools(&self) -> Vec<FullMCPTool> {
        let mut result = Vec::new();
        for (full_name, (server_name, tool_name)) in &self.tool_name_map {
            if let Some(entry) = self.servers.get(server_name)
                && let Some(tool) = entry.tools.iter().find(|t| t.name == *tool_name) {
                    result.push(FullMCPTool {
                        full_name: full_name.clone(),
                        tool: tool.clone(),
                    });
                }
        }
        result
    }

    pub fn get_servers_info(&self) -> Vec<ServerDisplayInfo> {
        let mut list: Vec<ServerDisplayInfo> = self.servers.iter().map(|(name, entry)| {
            let transport = match &entry.config {
                MCPServerConfig::Stdio { .. } => "stdio",
                MCPServerConfig::Http { .. } => "http",
                MCPServerConfig::Sse { .. } => "sse",
            }.to_string();
            ServerDisplayInfo {
                name: name.clone(),
                transport,
                status: match &entry.status {
                    MCPServerStatus::Pending => "pending".to_string(),
                    MCPServerStatus::Connecting => "connecting".to_string(),
                    MCPServerStatus::Connected => "connected".to_string(),
                    MCPServerStatus::AuthRequired(_) => "auth required".to_string(),
                    MCPServerStatus::Error(e) => format!("error: {e}"),
                    MCPServerStatus::Disabled => "disabled".to_string(),
                },
                tool_count: entry.tools.len(),
                resource_count: entry.resources.len(),
                enabled: entry.enabled,
                needs_auth: matches!(entry.status, MCPServerStatus::AuthRequired(_)),
            }
        }).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// The config + discovery hint needed to start an OAuth flow for `name`
    /// (`/mcp auth`'s Enter handler) — `None` if the server isn't known or
    /// isn't currently `AuthRequired`.
    pub fn oauth_context(&self, name: &str) -> Option<(MCPServerConfig, Option<String>)> {
        let entry = self.servers.get(name)?;
        match &entry.status {
            MCPServerStatus::AuthRequired(hint) => Some((entry.config.clone(), hint.clone())),
            _ => None,
        }
    }

    /// Whether a server named `name` is already configured — used by
    /// `/mcp add` to reject a duplicate before even attempting to connect.
    pub fn has_server(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    /// Add one new user-supplied server (from `/mcp add`'s paste-JSON,
    /// wizard, or quick inline syntax) as `ServerSource::Own`: connects it
    /// immediately and persists to `mcp.json`. Rejects a duplicate name
    /// outright rather than silently overwriting an existing server.
    ///
    /// Returns the server's status right after connecting — lets the
    /// caller report something more useful than "added" (e.g. "added, but
    /// requires authorization" or "added, but failed to connect: ...")
    /// without a second round trip through `get_servers_info`.
    pub async fn add_new_server(&mut self, name: String, config: MCPServerConfig) -> AppResult<MCPServerStatus> {
        if self.servers.contains_key(&name) {
            return Err(crate::error::AppError::Mcp(format!("Server '{name}' already exists")));
        }
        self.add_server(name.clone(), config, true, ServerSource::Own).await;
        if !self.servers.contains_key(&name) {
            // `add_server` silently no-ops when `build_client` fails (bad
            // stdio command, malformed URL, ...) — surface that here instead
            // of reporting success for a server that was never actually added.
            return Err(crate::error::AppError::Mcp(format!("Failed to create a client for '{name}'")));
        }
        self.connect_server(&name).await;
        self.persist_config().await;
        Ok(self.servers.get(&name).map(|e| e.status.clone()).unwrap_or(MCPServerStatus::Pending))
    }

    pub async fn toggle_server(&mut self, name: &str) -> AppResult<()> {
        let enabled = self.servers.get(name).map(|e| !e.enabled).unwrap_or(true);
        if let Some(entry) = self.servers.get_mut(name) {
            let config = entry.config.clone();
            entry.enabled = enabled;
            if enabled {
                // Recreate client and connect. The old client (a
                // `DummyTransport` if we're re-enabling a previously
                // disabled server, or a live one if toggling on twice in a
                // row without a reload) is replaced below — close it first
                // so a live stdio child doesn't leak.
                let old_client = Arc::clone(&entry.client);
                let client = build_client(name, &config).await?;
                if let Err(e) = old_client.close().await {
                    tracing::debug!("MCP '{name}' close before replace failed (best-effort): {e}");
                }
                entry.client = Arc::new(client);
                entry.status = MCPServerStatus::Pending;
                self.connect_server(name).await;
            } else {
                entry.status = MCPServerStatus::Disabled;
                entry.tools.clear();
                entry.resources.clear();
                // Close the live client before swapping in the dummy —
                // previously this just dropped the `MCPClient`, which for
                // stdio leaked the child process (no `Drop` impl ever
                // called `Transport::close()`, and `kill_on_drop` alone
                // only fires if the whole process tree is torn down, not on
                // a graceful drop-and-replace like this).
                let old_client = Arc::clone(&entry.client);
                if let Err(e) = old_client.close().await {
                    tracing::debug!("MCP '{name}' close on disable failed (best-effort): {e}");
                }
                entry.client = Arc::new(MCPClient::dummy(name));
                self.tool_name_map.retain(|_, (s, _)| s != name);
            }
        }
        self.persist_config().await;
        Ok(())
    }

    pub async fn reconnect_server(&mut self, name: &str) -> AppResult<()> {
        let config = self.servers.get(name).map(|e| e.config.clone());
        let Some(config) = config else {
            return Err(crate::error::AppError::Mcp(format!("Server '{name}' not found")));
        };
        let old_client = self.servers.get(name).map(|e| Arc::clone(&e.client));
        let client = build_client(name, &config).await?;
        if let Some(old_client) = old_client
            && let Err(e) = old_client.close().await {
                tracing::debug!("MCP '{name}' close before reconnect failed (best-effort): {e}");
            }
        // remove old tool mappings
        self.tool_name_map.retain(|_, (s, _)| s != name);
        if let Some(entry) = self.servers.get_mut(name) {
            entry.client = Arc::new(client);
            entry.enabled = true;
            entry.status = MCPServerStatus::Pending;
            entry.tools.clear();
            entry.resources.clear();
        }
        self.connect_server(name).await;
        self.persist_config().await;
        Ok(())
    }

    pub async fn remove_server(&mut self, name: &str) -> AppResult<()> {
        self.tool_name_map.retain(|_, (s, _)| s != name);
        if let Some(entry) = self.servers.remove(name)
            && let Err(e) = entry.client.close().await {
                tracing::debug!("MCP '{name}' close on remove failed (best-effort): {e}");
            }
        self.persist_config().await;
        Ok(())
    }

    /// Persist enabled/disabled state (and, for our own servers, full
    /// config) to `mcp.json`.
    ///
    /// Splits servers by `ServerSource` before handing off to
    /// `save_mcp_config`: `Own` servers go into `mcpServers` in full (same
    /// as before); `Foreign` servers — discovered from Claude Desktop, VS
    /// Code, Cursor, opencode, or the Claude CLI's own config files —
    /// contribute only their enabled bool to the `overrides` section.
    ///
    /// Previously every discovered server, foreign or not, was written into
    /// `mcpServers` in full on every single toggle — meaning any server
    /// from e.g. Claude Desktop's config (command, args, and any secrets in
    /// its `env` map) ended up duplicated into our own file the moment the
    /// user toggled *any* server in the UI, foreign or not.
    async fn persist_config(&self) {
        let mut own_servers = HashMap::new();
        let mut foreign_overrides = HashMap::new();
        for (name, entry) in &self.servers {
            match entry.source {
                ServerSource::Own => {
                    own_servers.insert(name.clone(), (entry.config.clone(), entry.enabled));
                }
                ServerSource::Foreign => {
                    foreign_overrides.insert(name.clone(), entry.enabled);
                }
            }
        }
        if let Err(e) = save_mcp_config(&own_servers, &foreign_overrides) {
            tracing::warn!("Failed to save MCP config: {e}");
        }
    }
}

/// Startup discovery + connect that never holds the manager lock across I/O.
///
/// `reload_all` (an explicit user action) runs under the caller's lock for
/// its whole duration; doing that at startup would freeze the UI in
/// disguise — the first frame now renders before any server has connected,
/// and event-loop paths (`system_prompt::build`, the /mcp view, the
/// resource picker) take `lock().await` on the manager. So this runs in
/// four phases and only ever locks for short, await-free merges:
///
/// 1. **no lock** — discover configs and build every enabled server's
///    client *concurrently* (`build_client` needs no manager state;
///    previously each `add_server` was awaited serially, so stdio spawns
///    and OAuth keyring lookups stacked per server);
/// 2. **short lock** — insert the entries (`Pending`/`Disabled`) so the
///    /mcp view can list them immediately, and snapshot connect inputs;
/// 3. **no lock** — run every `connect_one` concurrently (same job shape
///    as `connect_all`);
/// 4. **short lock** — merge the outcomes via `apply_connect_outcome`,
///    which skips servers the user removed/disabled meanwhile.
pub async fn startup_initialize(mcp: &tokio::sync::Mutex<MCPManager>) {
    let configs = load_mcp_config();
    let own_enabled = load_own_enabled_map();
    let foreign_overrides = load_overrides();

    // Phase 1: build all clients concurrently, off the lock.
    let jobs: Vec<_> = configs
        .into_iter()
        .map(|(name, config)| {
            let (enabled, source) =
                resolve_enabled_and_source(&name, &own_enabled, &foreign_overrides);
            async move {
                let client = if enabled {
                    match build_client(&name, &config).await {
                        Ok(c) => Some(c),
                        Err(e) => {
                            tracing::warn!("Failed to create MCP client for '{name}': {e}");
                            None
                        }
                    }
                } else {
                    None
                };
                (name, config, enabled, source, client)
            }
        })
        .collect();
    let built = futures::future::join_all(jobs).await;

    // Phase 2: insert entries and collect connect inputs in one short,
    // await-free lock hold.
    let (inputs, cache_ttl) = {
        let mut manager = mcp.lock().await;
        let mut inputs = Vec::new();
        for (name, config, enabled, source, client) in built {
            if !enabled {
                // Mirrors `add_server`'s disabled arm.
                manager.servers.insert(
                    name.clone(),
                    MCPServerEntry {
                        client: Arc::new(MCPClient::dummy(&name)),
                        config,
                        status: MCPServerStatus::Disabled,
                        tools: Vec::new(),
                        resources: Vec::new(),
                        enabled: false,
                        source,
                    },
                );
                continue;
            }
            // Mirrors `add_server`'s build-failure arm: silently skipped
            // (already logged in phase 1).
            let Some(client) = client else { continue };
            let client = Arc::new(client);
            manager.servers.insert(
                name.clone(),
                MCPServerEntry {
                    client: Arc::clone(&client),
                    config,
                    status: MCPServerStatus::Pending,
                    tools: Vec::new(),
                    resources: Vec::new(),
                    enabled: true,
                    source,
                },
            );
            let cached = manager.cache_snapshot(&name);
            inputs.push((name, client, cached));
        }
        (inputs, manager.cache_ttl)
    };

    // Phase 3: connect everything concurrently, no lock held.
    let jobs = inputs
        .into_iter()
        .map(|(name, client, cached)| connect_one(name, client, cached, cache_ttl));
    let outcomes = futures::future::join_all(jobs).await;

    // Phase 4: merge.
    let mut manager = mcp.lock().await;
    for outcome in outcomes {
        manager.apply_connect_outcome(outcome);
    }
}

/// Build a fresh `MCPClient` for `config`, dispatching to the right
/// transport constructor. Shared by `add_server` (initial connect),
/// `toggle_server` (re-enabling), and `reconnect_server` — previously each
/// duplicated the `Http`/`Sse` arms verbatim, which is exactly the kind of
/// duplication that made it easy to fix a bug in one copy and miss the
/// others.
///
/// For `Http`/`Sse`, merges in a stored OAuth `Authorization: Bearer` header
/// (`oauth_store::with_bearer_header`) if `/mcp auth` has ever completed a
/// flow for this server — best-effort and a no-op when there's no stored
/// token, so this is safe to call unconditionally for every server.
async fn build_client(name: &str, config: &MCPServerConfig) -> AppResult<MCPClient> {
    match config {
        MCPServerConfig::Stdio { command, args, env, cwd } => {
            MCPClient::from_stdio(name, command, args, env.as_ref(), cwd.as_deref()).await
        }
        MCPServerConfig::Http { url, headers } => {
            let headers = super::oauth_store::with_bearer_header(name, headers.clone()).await;
            MCPClient::from_http(name, url, headers).await
        }
        MCPServerConfig::Sse { url, headers } => {
            let headers = super::oauth_store::with_bearer_header(name, headers.clone()).await;
            MCPClient::from_sse(name, url, headers).await
        }
    }
}

pub struct FullMCPTool {
    pub full_name: String,
    pub tool: MCPTool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_enabled_and_source_own_server_uses_own_map() {
        let mut own = HashMap::new();
        own.insert("our-server".to_string(), false);
        let foreign = HashMap::new();

        let (enabled, source) = resolve_enabled_and_source("our-server", &own, &foreign);
        assert!(!enabled);
        assert_eq!(source, ServerSource::Own);
    }

    #[test]
    fn resolve_enabled_and_source_foreign_server_uses_override_map() {
        let own = HashMap::new();
        let mut foreign = HashMap::new();
        foreign.insert("claude-desktop-server".to_string(), false);

        let (enabled, source) = resolve_enabled_and_source("claude-desktop-server", &own, &foreign);
        assert!(!enabled);
        assert_eq!(source, ServerSource::Foreign);
    }

    #[test]
    fn resolve_enabled_and_source_foreign_server_defaults_enabled_without_override() {
        let own = HashMap::new();
        let foreign = HashMap::new();

        let (enabled, source) = resolve_enabled_and_source("never-toggled-server", &own, &foreign);
        assert!(enabled, "undiscovered foreign servers should default to enabled, matching prior behavior");
        assert_eq!(source, ServerSource::Foreign);
    }

    #[test]
    fn resolve_enabled_and_source_own_takes_priority_over_foreign_override() {
        // A name present in both maps (e.g. the user's own mcp.json happens
        // to reuse a name Claude Desktop also uses) must resolve as Own —
        // matching `load_mcp_config`'s discovery order, where our own
        // config is loaded first and wins via `entry().or_insert()`.
        let mut own = HashMap::new();
        own.insert("shared-name".to_string(), true);
        let mut foreign = HashMap::new();
        foreign.insert("shared-name".to_string(), false);

        let (enabled, source) = resolve_enabled_and_source("shared-name", &own, &foreign);
        assert!(enabled);
        assert_eq!(source, ServerSource::Own);
    }
}
