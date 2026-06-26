use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::error::AppResult;
use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};
use std::collections::HashMap;
use std::env;
use std::sync::Mutex;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse>;
    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()>;
    async fn close(&mut self) -> AppResult<()>;
    fn session_id(&self) -> Option<String> { None }
}

pub struct StdioTransport {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    reader: BufReader<tokio::process::ChildStdout>,
}

fn spawn_command(
    command: &str,
    args: &[String],
    env: Option<&HashMap<String, String>>,
    cwd: Option<&str>,
) -> AppResult<Child> {
    let build_cmd = |cmd_str: &str| -> Command {
        let mut cmd = Command::new(cmd_str);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        if let Some(env_map) = env {
            for (k, v) in env_map {
                cmd.env(k, v);
            }
        }
        cmd
    };

    let mut cmd = build_cmd(command);
    match cmd.spawn() {
        Ok(child) => return Ok(child),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && cfg!(target_os = "windows") => {
            // On Windows, executables like "npx" are often "npx.cmd" batch files.
            // Command::new("npx") doesn't auto-resolve via PATHEXT, so retry with .cmd.
            let cmd_ext = format!("{}.cmd", command);
            let mut cmd = build_cmd(&cmd_ext);
            match cmd.spawn() {
                Ok(child) => return Ok(child),
                Err(_) => {
                    // Also try .bat
                    let cmd_ext = format!("{}.bat", command);
                    let mut cmd = build_cmd(&cmd_ext);
                    cmd.spawn().map_err(|_| e)
                }
            }
        }
        Err(e) => Err(e.into()),
    }
    .map_err(|e| {
        let msg = format!(
            "Failed to spawn MCP subprocess '{}': {} (PATH: {})",
            command,
            e,
            env::var("PATH").unwrap_or_default()
        );
        crate::error::AppError::Mcp(msg)
    })
}

impl StdioTransport {
    pub async fn new(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
    ) -> AppResult<Self> {
        let mut child = spawn_command(command, args, env, cwd)?;
        let stdin = child.stdin.take()
            .ok_or_else(|| crate::error::AppError::Mcp("Failed to open stdin for MCP subprocess".to_string()))?;
        let stdout = child.stdout.take()
            .ok_or_else(|| crate::error::AppError::Mcp("Failed to open stdout for MCP subprocess".to_string()))?;
        let reader = BufReader::new(stdout);

        Ok(Self {
            child,
            stdin: Some(stdin),
            reader,
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        let mut json = serde_json::to_string(request)?;
        json.push('\n');

        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(json.as_bytes()).await?;
            stdin.flush().await?;
        }

        let mut line = String::new();
        timeout(Duration::from_secs(60), self.reader.read_line(&mut line))
            .await
            .map_err(|_| crate::error::AppError::Mcp("MCP request timed out after 60s".to_string()))??;

        let response: JsonRpcResponse = serde_json::from_str(&line)?;
        Ok(response)
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(data).await?;
            stdin.flush().await?;
        }
        Ok(())
    }

    async fn close(&mut self) -> AppResult<()> {
        drop(self.stdin.take());
        let _ = self.child.kill().await;
        Ok(())
    }
}

pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            url: url.to_string(),
            headers,
            session_id: Mutex::new(None),
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        let mut req = self.client.post(&self.url)
            .json(request)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(sid) = self.session_id.lock().unwrap().as_ref() {
            req = req.header("MCP-Session-Id", sid);
        }

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
            tracing::debug!("{} acquired session via MCP-Session-Id header", self.url);
        }

        let body = resp.text().await.unwrap_or_default();

        // Try JSON first
        if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&body) {
            return Ok(response);
        }

        // JSON failed — fallback to SSE parsing if body looks like SSE
        if body.contains("data:") || body.starts_with("event:") {
            tracing::debug!("HttpTransport: received SSE response, falling back to SSE parse for id={} at {}", request.id, self.url);
            return SseTransport::parse_sse_fallback(&body, request.id, &self.url, status).await;
        }

        let snippet = if body.len() > 200 { format!("{}...", &body[..200]) } else { body.clone() };
        Err(crate::error::AppError::Mcp(
            format!("HTTP MCP decode error (status={status}, url={}): body: {snippet}", self.url)
        ))
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        let mut req = self.client.post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(data.to_vec());

        if let Some(sid) = self.session_id.lock().unwrap().as_ref() {
            req = req.header("MCP-Session-Id", sid);
        }

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        req.send().await?;
        Ok(())
    }

    async fn close(&mut self) -> AppResult<()> {
        Ok(())
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }
}

pub struct SseTransport {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: Mutex<Option<String>>,
}

impl SseTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            url: url.to_string(),
            headers,
            session_id: Mutex::new(None),
        })
    }

    async fn parse_sse_stream(
        resp: reqwest::Response,
        request_id: u64,
        url: &str,
        status: reqwest::StatusCode,
    ) -> AppResult<JsonRpcResponse> {
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            loop {
                let event_end = match buffer.find("\n\n") {
                    Some(pos) => pos,
                    None => break,
                };

                let event_str = buffer[..event_end].to_string();
                buffer = buffer[event_end + 2..].to_string();

                let mut data_lines: Vec<String> = Vec::new();
                for line in event_str.lines() {
                    if let Some(d) = line.strip_prefix("data: ") {
                        data_lines.push(d.to_string());
                    } else if line.trim() == "data:" {
                        data_lines.push(String::new());
                    }
                }

                if data_lines.is_empty() {
                    continue;
                }

                let json_str = data_lines.join("\n");

                if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&json_str) {
                    if response.id == Some(request_id) {
                        return Ok(response);
                    }
                    tracing::debug!(
                        "SSE skipp no-matching id={:?} (waiting for id={request_id})",
                        response.id
                    );
                } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    if value.get("id").is_none() && value.get("method").is_some() {
                        tracing::debug!("SSE notification: {:?}", value);
                    }
                }
            }
        }

        Err(crate::error::AppError::Mcp(
            format!("SSE stream ended without matching response for id={request_id} (status={status}, url={url})")
        ))
    }

    async fn parse_json_body(
        resp: reqwest::Response,
        url: &str,
        status: reqwest::StatusCode,
    ) -> AppResult<JsonRpcResponse> {
        let body = resp.text().await.unwrap_or_default();
        serde_json::from_str(&body).map_err(|e| {
            let snippet = if body.len() > 200 {
                format!("{}...", &body[..200])
            } else {
                body.clone()
            };
            crate::error::AppError::Mcp(
                format!("HTTP MCP decode error (status={status}, url={url}): {e} — body: {snippet}")
            )
        })
    }
}

#[async_trait]
impl Transport for SseTransport {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        let mut req = self.client.post(&self.url)
            .json(request)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");

        if let Some(sid) = self.session_id.lock().unwrap().as_ref() {
            req = req.header("MCP-Session-Id", sid);
        }

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
            tracing::debug!("{} acquired session via MCP-Session-Id header", self.url);
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet = if body.len() > 200 { format!("{}...", &body[..200]) } else { body };
            return Err(crate::error::AppError::Mcp(
                format!("SSE transport error (status={status}, url={}): {snippet}", self.url)
            ));
        }

        let content_type = resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            Self::parse_sse_stream(resp, request.id, &self.url, status).await
        } else {
            // Try JSON first; if it fails and body looks like SSE, try SSE parse
            let body = resp.text().await.unwrap_or_default();
            let result = serde_json::from_str::<JsonRpcResponse>(&body);
            match result {
                Ok(r) => Ok(r),
                Err(json_err) => {
                    // Fallback: treat non-JSON body as SSE stream
                    if body.contains("data:") || body.contains("event:") {
                        tracing::debug!("SSE transport: non-SSE Content-Type but body looks like SSE, trying SSE parse");
                        Self::parse_sse_fallback(&body, request.id, &self.url, status).await
                    } else {
                        let snippet = if body.len() > 200 { format!("{}...", &body[..200]) } else { body };
                        Err(crate::error::AppError::Mcp(
                            format!("SSE transport decode error (status={status}, url={}): {json_err} — body: {snippet}", self.url)
                        ))
                    }
                }
            }
        }
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        let mut req = self.client.post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(data.to_vec());

        if let Some(sid) = self.session_id.lock().unwrap().as_ref() {
            req = req.header("MCP-Session-Id", sid);
        }

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        req.send().await?;
        Ok(())
    }

    async fn close(&mut self) -> AppResult<()> {
        Ok(())
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }
}

impl SseTransport {
    async fn parse_sse_fallback(
        body: &str,
        request_id: u64,
        url: &str,
        status: reqwest::StatusCode,
    ) -> AppResult<JsonRpcResponse> {
        let mut buffer = body.to_string();

        loop {
            let event_end = match buffer.find("\n\n") {
                Some(pos) => pos,
                None => break,
            };

            let event_str = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            let mut data_lines: Vec<String> = Vec::new();
            for line in event_str.lines() {
                if let Some(d) = line.strip_prefix("data: ") {
                    data_lines.push(d.to_string());
                } else if line.trim() == "data:" {
                    data_lines.push(String::new());
                }
            }

            if data_lines.is_empty() {
                continue;
            }

            let json_str = data_lines.join("\n");

            if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&json_str) {
                if response.id == Some(request_id) {
                    return Ok(response);
                }
            }
        }

        Err(crate::error::AppError::Mcp(
            format!("SSE fallback: no matching response for id={request_id} (status={status}, url={url})")
        ))
    }
}

pub struct DummyTransport;

#[async_trait]
impl Transport for DummyTransport {
    async fn send_request(&mut self, _request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        Err(crate::error::AppError::Mcp("Server is disabled".to_string()))
    }
    async fn send_raw(&mut self, _data: &[u8]) -> AppResult<()> {
        Err(crate::error::AppError::Mcp("Server is disabled".to_string()))
    }
    async fn close(&mut self) -> AppResult<()> {
        Ok(())
    }
}
