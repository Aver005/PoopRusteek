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
}

#[derive(Debug, Clone, PartialEq)]
pub enum MCPServerStatus {
    Pending,
    Connecting,
    Connected,
    Error(String),
    Disabled,
}

#[derive(Debug, Clone)]
pub struct MCPServer {
    pub name: String,
    pub config: MCPServerConfig,
    pub status: MCPServerStatus,
    pub tools: Vec<MCPTool>,
    pub resources: Vec<MCPResource>,
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
    pub mime_type: Option<String>,
    pub server_name: String,
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
