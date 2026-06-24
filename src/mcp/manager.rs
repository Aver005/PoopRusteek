use super::client::MCPClient;
use super::config::load_mcp_config;
use super::types::*;
use crate::error::AppResult;
use serde_json::Value;
use std::collections::HashMap;

pub struct MCPManager {
    servers: HashMap<String, MCPServerEntry>,
    tool_name_map: HashMap<String, (String, String)>,
}

struct MCPServerEntry {
    client: MCPClient,
    config: MCPServerConfig,
    status: MCPServerStatus,
    tools: Vec<MCPTool>,
    resources: Vec<MCPResource>,
}

impl MCPManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            tool_name_map: HashMap::new(),
        }
    }

    pub async fn initialize(&mut self) -> AppResult<()> {
        let configs = load_mcp_config();

        for (name, config) in configs {
            self.add_server(name, config).await;
        }

        self.connect_all().await;
        Ok(())
    }

    async fn add_server(&mut self, name: String, config: MCPServerConfig) {
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
        };

        if let Some(client) = client {
            self.servers.insert(name, MCPServerEntry {
                client,
                config,
                status: MCPServerStatus::Pending,
                tools: Vec::new(),
                resources: Vec::new(),
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

        match entry.client.list_tools().await {
            Ok(tools) => {
                for tool in &tools {
                    let full_name = format!("mcp__{}__{}", name, tool.name);
                    self.tool_name_map.insert(full_name, (name.to_string(), tool.name.clone()));
                }
                entry.tools = tools;
            }
            Err(e) => {
                tracing::warn!("MCP '{name}' list_tools failed: {e}");
            }
        }

        match entry.client.list_resources().await {
            Ok(resources) => {
                entry.resources = resources;
            }
            Err(e) => {
                tracing::warn!("MCP '{name}' list_resources failed: {e}");
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

    pub fn get_dynamic_tool_names(&self) -> Vec<String> {
        self.tool_name_map.keys().cloned().collect()
    }

    pub fn get_tool_description(&self, full_name: &str) -> Option<String> {
        let (server_name, tool_name) = self.tool_name_map.get(full_name)?;
        let entry = self.servers.get(server_name)?;
        let tool = entry.tools.iter().find(|t| t.name == *tool_name)?;
        Some(format!("{}: {}", tool.name, tool.description))
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

    pub fn server_status(&self, name: &str) -> Option<&MCPServerStatus> {
        self.servers.get(name).map(|e| &e.status)
    }

    pub fn disconnect_all(&mut self) {
        for entry in self.servers.values_mut() {
            entry.status = MCPServerStatus::Pending;
            entry.tools.clear();
            entry.resources.clear();
        }
        self.tool_name_map.clear();
    }
}

pub struct FullMCPTool {
    pub full_name: String,
    pub tool: MCPTool,
}
