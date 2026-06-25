use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::error::AppResult;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};
use std::collections::HashMap;
use std::env;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse>;
    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()>;
    async fn close(&mut self) -> AppResult<()>;
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
}

impl HttpTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            url: url.to_string(),
            headers,
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

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let response: JsonRpcResponse = serde_json::from_str(&body)
            .map_err(|e| {
                let snippet = if body.len() > 200 { format!("{}...", &body[..200]) } else { body.clone() };
                crate::error::AppError::Mcp(
                    format!("HTTP MCP decode error (status={status}, url={}): {e} — body: {snippet}", self.url)
                )
            })?;
        Ok(response)
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        let mut req = self.client.post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(data.to_vec());

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        req.send().await?;
        Ok(())
    }

    async fn close(&mut self) -> AppResult<()> {
        Ok(())
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
