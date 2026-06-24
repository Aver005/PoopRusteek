use super::types::*;
use crate::error::AppResult;
use std::collections::HashMap;

pub struct MCPManager {
    servers: HashMap<String, MCPServer>,
}

impl MCPManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    pub fn add_server(&mut self, name: String, config: MCPServerConfig) {
        self.servers.insert(name.clone(), MCPServer {
            name,
            config,
            status: MCPServerStatus::Pending,
            tools: Vec::new(),
            resources: Vec::new(),
        });
    }

    pub fn server_status(&self, name: &str) -> Option<&MCPServerStatus> {
        self.servers.get(name).map(|s| &s.status)
    }

    pub fn all_tools(&self) -> Vec<&MCPTool> {
        self.servers.values().flat_map(|s| &s.tools).collect()
    }

    pub fn all_resources(&self) -> Vec<&MCPResource> {
        self.servers.values().flat_map(|s| &s.resources).collect()
    }

    pub async fn connect_all(&mut self) -> AppResult<()> {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            if let Some(server) = self.servers.get_mut(&name) {
                server.status = MCPServerStatus::Connecting;
            }
            let result = self.try_connect(&name).await;
            if let Some(server) = self.servers.get_mut(&name) {
                match result {
                    Ok(()) => {
                        server.status = MCPServerStatus::Connected;
                        tracing::info!("MCP server '{name}' connected");
                    }
                    Err(e) => {
                        server.status = MCPServerStatus::Error(e.to_string());
                        tracing::warn!("MCP server '{name}' failed: {e}");
                    }
                }
            }
        }
        Ok(())
    }

    async fn try_connect(&self, _name: &str) -> AppResult<()> {
        // TODO: Implement actual MCP connection via stdio or HTTP
        Ok(())
    }

    pub fn disconnect_all(&mut self) {
        for server in self.servers.values_mut() {
            server.status = MCPServerStatus::Pending;
            server.tools.clear();
            server.resources.clear();
        }
    }
}
