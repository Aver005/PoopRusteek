use super::client::MCPClient;
use super::config::{load_mcp_config, load_own_enabled_map, save_mcp_config};
use super::types::*;
use crate::error::AppResult;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

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
    client: MCPClient,
    config: MCPServerConfig,
    status: MCPServerStatus,
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
    enabled: bool,
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

    pub async fn initialize(&mut self) -> AppResult<()> {
        let configs = load_mcp_config();
        let enabled_map = load_own_enabled_map();

        for (name, config) in configs {
            let enabled = enabled_map.get(&name).copied().unwrap_or(true);
            self.add_server(name, config, enabled).await;
        }

        self.connect_all().await;
        Ok(())
    }

    pub async fn reload_all(&mut self) {
        self.cache.clear();
        self.tool_name_map.clear();
        let configs = load_mcp_config();
        let enabled_map = load_own_enabled_map();
        self.servers.clear();

        for (name, config) in configs {
            let enabled = enabled_map.get(&name).copied().unwrap_or(true);
            self.add_server(name, config, enabled).await;
        }

        self.connect_all().await;
    }

    async fn add_server(&mut self, name: String, config: MCPServerConfig, enabled: bool) {
        if !enabled {
            let entry = MCPServerEntry {
                client: MCPClient::dummy(&name),
                config,
                status: MCPServerStatus::Disabled,
                tools: Vec::new(),
                resources: Vec::new(),
                enabled: false,
            };
            self.servers.insert(name, entry);
            return;
        }
        let client = match &config {
            MCPServerConfig::Stdio { command, args, env, cwd } => {
                match MCPClient::from_stdio(
                    &name,
                    command,
                    args,
                    env.as_ref(),
                    cwd.as_deref(),
                ).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("Failed to create MCP client for '{name}': {e}");
                        None
                    }
                }
            }
            MCPServerConfig::Http { url, headers } => {
                match MCPClient::from_http(&name, url, headers.clone()).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("Failed to create MCP client for '{name}': {e}");
                        None
                    }
                }
            }
            MCPServerConfig::Sse { url, headers } => {
                match MCPClient::from_sse(&name, url, headers.clone()).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("Failed to create MCP client for '{name}': {e}");
                        None
                    }
                }
            }
        };

        if let Some(client) = client {
            self.servers.insert(name, MCPServerEntry {
                client,
                config,
                status: MCPServerStatus::Pending,
                tools: Vec::new(),
                resources: Vec::new(),
                enabled: true,
            });
        }
    }

    async fn connect_all(&mut self) {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            self.connect_server(&name).await;
        }
    }

    async fn connect_server(&mut self, name: &str) {
        let entry = match self.servers.get_mut(name) {
            Some(e) => e,
            None => return,
        };

        entry.status = MCPServerStatus::Connecting;

        match entry.client.initialize().await {
            Ok(_caps) => {
                if let Err(e) = entry.client.send_initialized().await {
                    entry.status = MCPServerStatus::Error(e.to_string());
                    tracing::warn!("MCP '{name}' initialized but send_initialized failed: {e}");
                    return;
                }
            }
            Err(e) => {
                entry.status = MCPServerStatus::Error(e.to_string());
                tracing::warn!("MCP '{name}' initialize failed: {e}");
                return;
            }
        }

        // Check cache
        if let Some(cache) = self.cache.get(name) {
            let elapsed = cache.cached_at.elapsed().as_secs();
            if elapsed < self.cache_ttl {
                tracing::info!("MCP '{name}' using cache ({elapsed}s old, TTL {})", self.cache_ttl);
                entry.tools = cache.tools.clone();
                entry.resources = cache.resources.clone();
                for (full_name, server_name, tool_name) in &cache.tool_entries {
                    self.tool_name_map.insert(full_name.clone(), (server_name.clone(), tool_name.clone()));
                }
                entry.status = MCPServerStatus::Connected;
                return;
            }
            tracing::info!("MCP '{name}' cache expired ({elapsed}s >= TTL {}s)", self.cache_ttl);
        }

        // Fetch from server
        match entry.client.list_tools().await {
            Ok(tools) => {
                let mut tool_entries = Vec::new();
                for tool in &tools {
                    let full_name = format!("mcp__{}__{}", name, tool.name);
                    tool_entries.push((full_name.clone(), name.to_string(), tool.name.clone()));
                    self.tool_name_map.insert(full_name, (name.to_string(), tool.name.clone()));
                }
                entry.tools = tools.clone();

                let resources = match entry.client.list_resources().await {
                    Ok(r) => {
                        tracing::info!("MCP '{name}' fetched {} resources", r.len());
                        r
                    }
                    Err(e) => {
                        tracing::info!("MCP '{name}' list_resources (optional): {e}");
                        Vec::new()
                    }
                };
                entry.resources = resources.clone();

                self.cache.insert(name.to_string(), ServerCache {
                    tools,
                    resources,
                    tool_entries,
                    cached_at: Instant::now(),
                });
            }
            Err(e) => {
                tracing::warn!("MCP '{name}' list_tools failed: {e}");
                let _ = entry.client.list_resources().await;
            }
        }

        entry.status = MCPServerStatus::Connected;
        tracing::info!("MCP '{name}' connected with {} tools", entry.tools.len());
    }

    pub async fn call_tool(&mut self, full_tool_name: &str, args: Value) -> AppResult<MCPToolResult> {
        let (server_name, tool_name) = self.tool_name_map.get(full_tool_name)
            .ok_or_else(|| crate::error::AppError::Mcp(format!("Unknown MCP tool: {full_tool_name}")))?;

        let server_name = server_name.clone();
        let tool_name = tool_name.clone();

        let entry = self.servers.get_mut(&server_name)
            .ok_or_else(|| crate::error::AppError::Mcp(format!("Server not found: {server_name}")))?;

        if !matches!(entry.status, MCPServerStatus::Connected) {
            return Ok(MCPToolResult {
                content: format!("MCP server '{server_name}' is not connected"),
                is_error: true,
            });
        }

        entry.client.call_tool(&tool_name, args).await
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
            if let Some(entry) = self.servers.get(server_name) {
                if let Some(tool) = entry.tools.iter().find(|t| t.name == *tool_name) {
                    result.push(FullMCPTool {
                        full_name: full_name.clone(),
                        tool: tool.clone(),
                    });
                }
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
                    MCPServerStatus::Error(e) => format!("error: {e}"),
                    MCPServerStatus::Disabled => "disabled".to_string(),
                },
                tool_count: entry.tools.len(),
                resource_count: entry.resources.len(),
                enabled: entry.enabled,
            }
        }).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub async fn toggle_server(&mut self, name: &str) -> AppResult<()> {
        let enabled = self.servers.get(name).map(|e| !e.enabled).unwrap_or(true);
        if let Some(entry) = self.servers.get_mut(name) {
            let config = entry.config.clone();
            entry.enabled = enabled;
            if enabled {
                // recreate client and connect
                let client = match &config {
                    MCPServerConfig::Stdio { command, args, env, cwd } => {
                        MCPClient::from_stdio(name, command, args, env.as_ref(), cwd.as_deref()).await?
                    }
                    MCPServerConfig::Http { url, headers } => {
                        MCPClient::from_http(name, url, headers.clone()).await?
                    }
                    MCPServerConfig::Sse { url, headers } => {
                        MCPClient::from_sse(name, url, headers.clone()).await?
                    }
                };
                entry.client = client;
                entry.status = MCPServerStatus::Pending;
                self.connect_server(name).await;
            } else {
                entry.status = MCPServerStatus::Disabled;
                entry.tools.clear();
                entry.resources.clear();
                entry.client = MCPClient::dummy(name);
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
        let enabled = true;
        let client = match &config {
            MCPServerConfig::Stdio { command, args, env, cwd } => {
                MCPClient::from_stdio(name, command, args, env.as_ref(), cwd.as_deref()).await?
            }
            MCPServerConfig::Http { url, headers } => {
                MCPClient::from_http(name, url, headers.clone()).await?
            }
            MCPServerConfig::Sse { url, headers } => {
                MCPClient::from_sse(name, url, headers.clone()).await?
            }
        };
        // remove old tool mappings
        self.tool_name_map.retain(|_, (s, _)| s != name);
        if let Some(entry) = self.servers.get_mut(name) {
            entry.client = client;
            entry.enabled = enabled;
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
        self.servers.remove(name);
        self.persist_config().await;
        Ok(())
    }

    async fn persist_config(&self) {
        let servers: HashMap<String, (MCPServerConfig, bool)> = self.servers.iter()
            .map(|(name, entry)| (name.clone(), (entry.config.clone(), entry.enabled)))
            .collect();
        if let Err(e) = save_mcp_config(&servers) {
            tracing::warn!("Failed to save MCP config: {e}");
        }
    }
}

pub struct FullMCPTool {
    pub full_name: String,
    pub tool: MCPTool,
}
