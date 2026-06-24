use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::error::AppResult;
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use std::collections::HashMap;

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

impl StdioTransport {
    pub async fn new(
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
    ) -> AppResult<Self> {
        let mut cmd = Command::new(command);
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

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
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
        self.reader.read_line(&mut line).await?;

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
            .header("Content-Type", "application/json");

        for (k, v) in &self.headers {
            req = req.header(k, v);
        }

        let resp = req.send().await?;
        let response: JsonRpcResponse = resp.json().await?;
        Ok(response)
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        let mut req = self.client.post(&self.url)
            .header("Content-Type", "application/json")
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
