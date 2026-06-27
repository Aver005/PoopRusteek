use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse, JsonRpcNotification};
use super::transport::{Transport, StdioTransport, HttpTransport, SseTransport};
use super::types::*;
use crate::error::AppResult;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct MCPClient {
    transport: Arc<Mutex<dyn Transport>>,
    server_name: String,
    next_id: u64,
}

impl MCPClient {
    pub async fn from_stdio(
        server_name: &str,
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
    ) -> AppResult<Self> {
        let transport = StdioTransport::new(command, args, env, cwd).await?;
        Ok(Self {
            transport: Arc::new(Mutex::new(transport)),
            server_name: server_name.to_string(),
            next_id: 1,
        })
    }

    pub fn dummy(server_name: &str) -> Self {
        Self {
            transport: Arc::new(Mutex::new(super::transport::DummyTransport)),
            server_name: server_name.to_string(),
            next_id: 0,
        }
    }

    pub async fn from_http(
        server_name: &str,
        url: &str,
        headers: HashMap<String, String>,
    ) -> AppResult<Self> {
        let transport = HttpTransport::new(url, headers)?;
        Ok(Self {
            transport: Arc::new(Mutex::new(transport)),
            server_name: server_name.to_string(),
            next_id: 1,
        })
    }

    pub async fn from_sse(
        server_name: &str,
        url: &str,
        headers: HashMap<String, String>,
    ) -> AppResult<Self> {
        let transport = SseTransport::new(url, headers)?;
        Ok(Self {
            transport: Arc::new(Mutex::new(transport)),
            server_name: server_name.to_string(),
            next_id: 1,
        })
    }

    pub async fn initialize(&mut self) -> AppResult<ServerCapabilities> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "pooprusteek",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let response = self.call_impl("initialize", Some(params)).await?;

        // Log session ID from transport (set via MCP-Session-Id HTTP header)
        let transport = self.transport.lock().await;
        if let Some(sid) = transport.session_id() {
            tracing::debug!("MCP '{}' session: {}", self.server_name, sid);
        } else {
            tracing::debug!("MCP '{}': no session ID from transport", self.server_name);
        }
        drop(transport);

        let caps: ServerCapabilities = serde_json::from_value(
            response.result.unwrap_or(json!({}))
        )?;
        Ok(caps)
    }

    pub async fn send_initialized(&mut self) -> AppResult<()> {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
            _meta: None,
        };
        let json = serde_json::to_string(&notification)?;
        let mut data = json.into_bytes();
        data.push(b'\n');

        let mut transport = self.transport.lock().await;
        transport.send_raw(&data).await
    }

    pub async fn list_tools(&mut self) -> AppResult<Vec<MCPTool>> {
        let response = self.call("tools/list", None).await?;
        let result = response.result.unwrap_or(json!({}));
        let tools_value = result.get("tools").cloned().unwrap_or(json!([]));
        let raw_tools: Vec<MCPToolRaw> = serde_json::from_value(tools_value)?;
        let tools = raw_tools.into_iter().map(|t| MCPTool {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
            server_name: self.server_name.clone(),
        }).collect();
        Ok(tools)
    }

    pub async fn list_resources(&mut self) -> AppResult<Vec<MCPResource>> {
        let response = self.call("resources/list", None).await?;
        let result = response.result.unwrap_or(json!({}));
        let resources_value = result.get("resources").cloned().unwrap_or(json!([]));
        let raw_resources: Vec<MCPResourceRaw> = serde_json::from_value(resources_value)?;
        let resources = raw_resources.into_iter().map(|r| MCPResource {
            uri: r.uri,
            name: r.name,
            description: r.description,
        }).collect();
        Ok(resources)
    }

    pub async fn call_tool(&mut self, tool_name: &str, args: Value) -> AppResult<MCPToolResult> {
        let params = json!({
            "name": tool_name,
            "arguments": args
        });

        let response = self.call("tools/call", Some(params)).await?;

        if let Some(error) = &response.error {
            return Ok(MCPToolResult {
                content: error.message.clone(),
                is_error: true,
            });
        }

        let result = response.result.unwrap_or(json!({}));
        let content = result.get("content").cloned().unwrap_or(json!([]));
        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);

        let text = flatten_content(&content);

        Ok(MCPToolResult {
            content: text,
            is_error,
        })
    }

    async fn call(&mut self, method: &str, params: Option<Value>) -> AppResult<JsonRpcResponse> {
        self.call_impl(method, params).await
    }

    async fn call_impl(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> AppResult<JsonRpcResponse> {
        let id = self.next_id;
        self.next_id += 1;

        let request = JsonRpcRequest::new(id, method, params);

        let mut transport = self.transport.lock().await;
        let response = transport.send_request(&request).await?;

        if let Some(err) = &response.error {
            return Err(crate::error::AppError::Mcp(
                format!("JSON-RPC error (method={method}): code={}, message={}", err.code, err.message)
            ));
        }

        Ok(response)
    }
}

#[derive(Debug, serde::Deserialize)]
struct MCPToolRaw {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, serde::Deserialize)]
struct MCPResourceRaw {
    uri: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ContentItem {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
}

fn flatten_content(content: &Value) -> String {
    let items: Vec<ContentItem> = serde_json::from_value(content.clone()).unwrap_or_default();
    let mut result = String::new();

    for item in items {
        match item.content_type.as_str() {
            "text" => {
                if let Some(text) = &item.text {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(text);
                }
            }
            "image" => {
                if !result.is_empty() {
                    result.push('\n');
                }
                let mime = item.mime_type.as_deref().unwrap_or("unknown");
                result.push_str(&format!("[Image: {mime}]"));
            }
            "resource" => {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("[Resource]");
            }
            _ => {}
        }
    }

    result
}
