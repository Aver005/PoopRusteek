//! JSON-RPC session for a single MCP server, independent of transport.
//!
//! `MCPClient` wraps a `Transport` (stdio/HTTP/SSE) with the MCP method
//! calls (`initialize`, `tools/list`, `tools/call`, ...) and per-call id
//! bookkeeping. Every method takes `&self`: `transport` is already an
//! `Arc<Mutex<dyn Transport>>` and `next_id` is an atomic counter, so there
//! is no `&mut self`-shaped state left. This is what lets `MCPManager` hand
//! out `Arc<MCPClient>` handles (`MCPManager::client_for`) and run a tool
//! call without holding the manager's own lock across the network await —
//! previously that lock was held for the whole call, freezing the UI event
//! loop for the duration of every MCP tool invocation.

use super::jsonrpc::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::{HttpTransport, SseTransport, StdioTransport, Transport};
use super::types::*;
use crate::error::AppResult;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

pub struct MCPClient {
    transport: Arc<Mutex<dyn Transport>>,
    server_name: String,
    next_id: AtomicU64,
}

impl MCPClient {
    pub async fn from_stdio(
        server_name: &str,
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
    ) -> AppResult<Self> {
        let transport = StdioTransport::new(server_name, command, args, env, cwd).await?;
        Ok(Self {
            transport: Arc::new(Mutex::new(transport)),
            server_name: server_name.to_string(),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn dummy(server_name: &str) -> Self {
        Self {
            transport: Arc::new(Mutex::new(super::transport::DummyTransport)),
            server_name: server_name.to_string(),
            next_id: AtomicU64::new(0),
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
            next_id: AtomicU64::new(1),
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
            next_id: AtomicU64::new(1),
        })
    }

    pub async fn initialize(&self) -> AppResult<ServerCapabilities> {
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

        let caps: ServerCapabilities =
            serde_json::from_value(response.result.unwrap_or(json!({})))?;
        Ok(caps)
    }

    pub async fn send_initialized(&self) -> AppResult<()> {
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

    pub async fn list_tools(&self) -> AppResult<Vec<MCPTool>> {
        let response = self.call("tools/list", None).await?;
        let result = response.result.unwrap_or(json!({}));
        let tools_value = result.get("tools").cloned().unwrap_or(json!([]));
        let raw_tools: Vec<MCPToolRaw> = serde_json::from_value(tools_value)?;
        let tools = raw_tools
            .into_iter()
            .map(|t| MCPTool {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                server_name: self.server_name.clone(),
            })
            .collect();
        Ok(tools)
    }

    pub async fn list_resources(&self) -> AppResult<Vec<MCPResource>> {
        let response = self.call("resources/list", None).await?;
        let result = response.result.unwrap_or(json!({}));
        let resources_value = result.get("resources").cloned().unwrap_or(json!([]));
        let raw_resources: Vec<MCPResourceRaw> = serde_json::from_value(resources_value)?;
        let resources = raw_resources
            .into_iter()
            .map(|r| MCPResource {
                uri: r.uri,
                name: r.name,
                description: r.description,
            })
            .collect();
        Ok(resources)
    }

    /// A transport-level failure (I/O error, timeout, decode error — see
    /// `call_impl`) surfaces as `Err`; a tool that ran but reported failure
    /// (JSON-RPC error response, or a successful response with
    /// `isError: true`) surfaces as `Ok(MCPToolResult { is_error: true, .. })`.
    /// Callers that track connection health (`MCPManager`) should treat only
    /// the `Err` case as evidence the *connection* is unhealthy.
    pub async fn call_tool(&self, tool_name: &str, args: Value) -> AppResult<MCPToolResult> {
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
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let text = flatten_content(&content);

        Ok(MCPToolResult {
            content: text,
            is_error,
        })
    }

    async fn call(&self, method: &str, params: Option<Value>) -> AppResult<JsonRpcResponse> {
        self.call_impl(method, params).await
    }

    async fn call_impl(&self, method: &str, params: Option<Value>) -> AppResult<JsonRpcResponse> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = JsonRpcRequest::new(id, method, params);

        let mut transport = self.transport.lock().await;
        let response = transport.send_request(&request).await?;

        if let Some(err) = &response.error {
            return Err(crate::error::AppError::Mcp(format!(
                "JSON-RPC error (method={method}): code={}, message={}",
                err.code, err.message
            )));
        }

        Ok(response)
    }

    /// Close the underlying transport (kills the child process for stdio;
    /// no-op for HTTP/SSE). Callers holding only a shared `Arc<MCPClient>`
    /// (e.g. via `MCPManager::client_for`) still need a way to shut the
    /// transport down on manager reload/disable/remove, hence `&self`.
    pub async fn close(&self) -> AppResult<()> {
        let mut transport = self.transport.lock().await;
        transport.close().await
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_content_single_text_item() {
        let content = json!([{ "type": "text", "text": "hello" }]);
        assert_eq!(flatten_content(&content), "hello");
    }

    #[test]
    fn flatten_content_joins_multiple_items_with_newline() {
        let content = json!([
            { "type": "text", "text": "line one" },
            { "type": "text", "text": "line two" }
        ]);
        assert_eq!(flatten_content(&content), "line one\nline two");
    }

    #[test]
    fn flatten_content_image_uses_mime_type() {
        let content = json!([{ "type": "image", "mimeType": "image/png" }]);
        assert_eq!(flatten_content(&content), "[Image: image/png]");
    }

    #[test]
    fn flatten_content_image_missing_mime_defaults_unknown() {
        let content = json!([{ "type": "image" }]);
        assert_eq!(flatten_content(&content), "[Image: unknown]");
    }

    #[test]
    fn flatten_content_resource_placeholder() {
        let content = json!([{ "type": "resource" }]);
        assert_eq!(flatten_content(&content), "[Resource]");
    }

    #[test]
    fn flatten_content_unknown_type_ignored() {
        let content = json!([
            { "type": "text", "text": "kept" },
            { "type": "some-future-type" }
        ]);
        assert_eq!(flatten_content(&content), "kept");
    }

    #[test]
    fn flatten_content_empty_array_is_empty_string() {
        assert_eq!(flatten_content(&json!([])), "");
    }

    #[test]
    fn flatten_content_malformed_input_falls_back_to_empty() {
        // Not an array of ContentItem — `unwrap_or_default()` should absorb
        // this rather than panicking.
        assert_eq!(flatten_content(&json!({"not": "a list"})), "");
    }
}
