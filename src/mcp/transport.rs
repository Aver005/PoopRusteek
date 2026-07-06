//! Wire transports for MCP: stdio (child process), HTTP (Streamable HTTP),
//! and SSE. Each implements the `Transport` trait's `send_request`, which
//! turns one `JsonRpcRequest` into one matching `JsonRpcResponse` — the
//! "matching" part is the interesting bit for stdio and SSE, since both can
//! interleave notifications and server-initiated requests with the actual
//! response on the same stream (see `send_request` on `StdioTransport` and
//! `parse_sse_stream`/`parse_sse_fallback` on `SseTransport`).

use super::jsonrpc::{JsonRpcRequest, JsonRpcResponse, ids_match, is_notification_or_request};
use crate::error::AppResult;
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashMap;
use std::env;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{Duration, Instant, timeout};

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse>;
    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()>;
    async fn close(&mut self) -> AppResult<()>;
    fn session_id(&self) -> Option<String> {
        None
    }
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
            .stderr(std::process::Stdio::piped())
            // Without this, dropping the `Child` (e.g. on `MCPManager` reload
            // or replacement) leaves the subprocess running as an orphan —
            // tokio only kills on drop if explicitly told to.
            .kill_on_drop(true);
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
        Err(e) => Err(e),
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
    /// `server_name` tags the drained stderr log lines (`mcp_stderr` target)
    /// so they're attributable when multiple servers run concurrently.
    pub async fn new(
        server_name: &str,
        command: &str,
        args: &[String],
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
    ) -> AppResult<Self> {
        let mut child = spawn_command(command, args, env, cwd)?;
        let stdin = child.stdin.take().ok_or_else(|| {
            crate::error::AppError::Mcp("Failed to open stdin for MCP subprocess".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            crate::error::AppError::Mcp("Failed to open stdout for MCP subprocess".to_string())
        })?;
        let reader = BufReader::new(stdout);

        // The child's stderr pipe has a small OS buffer (~64KB on most
        // platforms). If nothing ever reads it and the server writes enough
        // diagnostic output, the pipe fills and the server blocks on its next
        // stderr write — which for many MCP servers happens on the same
        // thread/loop that would otherwise answer our stdout request, so the
        // whole connection deadlocks. Drain it continuously into tracing so
        // it can never back up, and so operators get server diagnostics for
        // free.
        //
        // Read raw bytes (`read_until(b'\n', ..)`) rather than
        // `AsyncBufReadExt::lines()`: `lines()` validates UTF-8 per line and
        // yields `Err` on invalid sequences, which would stop this loop dead
        // and silently reopen the exact pipe-fills-and-blocks deadlock this
        // task exists to prevent. `from_utf8_lossy` never fails, so the drain
        // loop can only stop on a real I/O error or EOF.
        if let Some(stderr) = child.stderr.take() {
            let name = server_name.to_string();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) => break, // EOF: child closed stderr (usually means it exited)
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&buf);
                            let line = line.trim_end_matches(['\r', '\n']);
                            if !line.is_empty() {
                                tracing::debug!(target: "mcp_stderr", server = %name, "{line}");
                            }
                        }
                        Err(e) => {
                            tracing::debug!(target: "mcp_stderr", server = %name, "stderr read error: {e}");
                            break;
                        }
                    }
                }
            });
        }

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

        // A stdio server multiplexes everything onto stdout: our response,
        // plus any notifications (`notifications/progress`, log messages)
        // and server-initiated requests it feels like sending in between.
        // Reading exactly one line and trusting it blind means a stray
        // notification gets returned as our answer and every subsequent
        // real response is then off by one. Loop until a line parses as a
        // JSON-RPC message whose `id` matches this request; skip (and log)
        // anything else. The 60s budget covers the *entire* wait, not each
        // individual line — a chatty-but-slow server can't reset the clock
        // by sending an endless stream of notifications.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            // `Instant::duration_since` on `tokio::time::Instant` returns a
            // zero `Duration` (rather than panicking or requiring an
            // `Option` unwrap) when `self` is already in the past relative
            // to the argument — exactly the saturating-at-zero behavior
            // needed here once the deadline has passed.
            let remaining = deadline.duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(crate::error::AppError::Mcp(
                    "MCP request timed out after 60s".to_string(),
                ));
            }

            let mut line = String::new();
            let read = timeout(remaining, self.reader.read_line(&mut line)).await;
            let bytes_read = match read {
                Ok(inner) => inner?,
                Err(_) => {
                    return Err(crate::error::AppError::Mcp(
                        "MCP request timed out after 60s".to_string(),
                    ));
                }
            };
            if bytes_read == 0 {
                return Err(crate::error::AppError::Mcp(
                    "MCP subprocess closed stdout before sending a response".to_string(),
                ));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("MCP stdio: skipping non-JSON line on stdout: {e} ({trimmed})");
                    continue;
                }
            };

            if is_notification_or_request(&value) {
                tracing::debug!(
                    "MCP stdio: skipping notification/server-request while awaiting id={}: {value}",
                    request.id
                );
                continue;
            }

            let response: JsonRpcResponse = match serde_json::from_value(value.clone()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("MCP stdio: skipping unparseable response line: {e} ({value})");
                    continue;
                }
            };

            match &response.id {
                Some(id) if ids_match(request.id, id) => return Ok(response),
                other => {
                    tracing::debug!(
                        "MCP stdio: skipping response with non-matching id={:?} (waiting for id={})",
                        other,
                        request.id
                    );
                    continue;
                }
            }
        }
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
        let client = build_http_client()?;

        Ok(Self {
            client,
            url: url.to_string(),
            headers,
            session_id: Mutex::new(None),
        })
    }
}

/// Shared timeout policy for `HttpTransport` and `SseTransport`.
///
/// Previously both used a flat 30s *total* timeout (connect + send + full
/// response), which is a much tighter budget than stdio's 60s and hard-cuts
/// any tool call — or SSE body — that legitimately runs long. This aligns
/// the per-request budget with stdio's 60s while giving connection setup its
/// own short 10s allowance, so a slow-to-connect server doesn't eat into the
/// time budgeted for the actual call. `read_timeout` additionally caps the
/// gap between individual reads of a streaming (SSE) body — a stalled
/// stream with no bytes for 60s is treated the same as a stalled non-stream
/// response, rather than being able to hang indefinitely past the request
/// timeout by trickling bytes just often enough (reqwest supports both
/// knobs directly on `ClientBuilder`, so there is no residual gap to
/// document here).
fn build_http_client() -> AppResult<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .cookie_store(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// Attach the sticky `MCP-Session-Id` (if one was captured from an earlier
/// response) plus any static per-server headers from config. Shared by every
/// `send_request`/`send_raw` on both `HttpTransport` and `SseTransport` — the
/// two previously each duplicated this arm-for-arm across all four methods.
fn apply_common_headers(
    mut req: reqwest::RequestBuilder,
    session_id: &Mutex<Option<String>>,
    headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    if let Some(sid) = session_id.lock().unwrap().as_ref() {
        req = req.header("MCP-Session-Id", sid);
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req
}

/// If `resp` is a 401, turn it into `AppError::McpAuthRequired` carrying the
/// `WWW-Authenticate` header (if any). `Ok(())` for every other status —
/// callers still handle non-2xx/non-401 statuses themselves, since
/// `HttpTransport` and `SseTransport` react to those differently (the former
/// falls through to body-sniffing, the latter treats any non-2xx as a hard
/// error before even looking at the body).
fn auth_required_error(resp: &reqwest::Response) -> Option<crate::error::AppError> {
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return None;
    }
    let www_authenticate = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Some(crate::error::AppError::McpAuthRequired { www_authenticate })
}

/// Capture a fresh `MCP-Session-Id` response header into `session_id`, if
/// present. Shared by both transports' `send_request`.
fn capture_session_id(resp: &reqwest::Response, session_id: &Mutex<Option<String>>, url: &str) {
    if let Some(sid) = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        *session_id.lock().unwrap() = Some(sid.to_string());
        tracing::debug!("{url} acquired session via MCP-Session-Id header");
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        let req = self
            .client
            .post(&self.url)
            .json(request)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        let req = apply_common_headers(req, &self.session_id, &self.headers);

        let resp = req.send().await?;
        let status = resp.status();

        if let Some(err) = auth_required_error(&resp) {
            return Err(err);
        }
        capture_session_id(&resp, &self.session_id, &self.url);

        let body = resp.text().await.map_err(|e| {
            crate::error::AppError::Mcp(format!(
                "Failed to read HTTP response body at {}: {e}",
                self.url
            ))
        })?;

        // Try JSON first. Unlike stdio, a single POST here maps to a single
        // response body — there's no interleaved-notifications stream to
        // filter — but still guard against a server that mistakenly echoes
        // a notification/request shape back as the "response".
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
            && !is_notification_or_request(&value)
            && let Ok(response) = serde_json::from_value::<JsonRpcResponse>(value)
        {
            return Ok(response);
        }

        // JSON failed — fallback to SSE parsing if body looks like SSE
        if body.contains("data:") || body.starts_with("event:") {
            tracing::debug!(
                "HttpTransport: received SSE response, falling back to SSE parse for id={} at {}",
                request.id,
                self.url
            );
            return SseTransport::parse_sse_fallback(&body, request.id, &self.url, status).await;
        }

        let snippet = if body.len() > 200 {
            crate::util::truncate_with_ellipsis(&body, 200)
        } else {
            body.clone()
        };
        Err(crate::error::AppError::Mcp(format!(
            "HTTP MCP decode error (status={status}, url={}): body: {snippet}",
            self.url
        )))
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        let req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(data.to_vec());
        let req = apply_common_headers(req, &self.session_id, &self.headers);

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
        let client = build_http_client()?;

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

            while let Some(event_end) = buffer.find("\n\n") {
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

                let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) else {
                    continue;
                };

                if is_notification_or_request(&value) {
                    tracing::debug!(
                        "SSE notification/server-request while awaiting id={request_id}: {value}"
                    );
                    continue;
                }

                let Ok(response) = serde_json::from_value::<JsonRpcResponse>(value) else {
                    continue;
                };

                match &response.id {
                    Some(id) if ids_match(request_id, id) => return Ok(response),
                    other => {
                        tracing::debug!(
                            "SSE skipping non-matching id={:?} (waiting for id={request_id})",
                            other
                        );
                    }
                }
            }
        }

        Err(crate::error::AppError::Mcp(format!(
            "SSE stream ended without matching response for id={request_id} (status={status}, url={url})"
        )))
    }
}

#[async_trait]
impl Transport for SseTransport {
    async fn send_request(&mut self, request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        let req = self
            .client
            .post(&self.url)
            .json(request)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");
        let req = apply_common_headers(req, &self.session_id, &self.headers);

        let resp = req.send().await?;
        let status = resp.status();

        if let Some(err) = auth_required_error(&resp) {
            return Err(err);
        }
        capture_session_id(&resp, &self.session_id, &self.url);

        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| format!("(failed to read body, status={status})"));
            let snippet = if body.len() > 200 {
                crate::util::truncate_with_ellipsis(&body, 200)
            } else {
                body
            };
            return Err(crate::error::AppError::Mcp(format!(
                "SSE transport error (status={status}, url={}): {snippet}",
                self.url
            )));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            Self::parse_sse_stream(resp, request.id, &self.url, status).await
        } else {
            // Try JSON first; if it fails and body looks like SSE, try SSE parse
            let body = resp.text().await.map_err(|e| {
                crate::error::AppError::Mcp(format!(
                    "Failed to read SSE response body at {}: {e}",
                    self.url
                ))
            })?;
            let result = serde_json::from_str::<JsonRpcResponse>(&body);
            match result {
                Ok(r) => Ok(r),
                Err(json_err) => {
                    // Fallback: treat non-JSON body as SSE stream
                    if body.contains("data:") || body.contains("event:") {
                        tracing::debug!(
                            "SSE transport: non-SSE Content-Type but body looks like SSE, trying SSE parse"
                        );
                        Self::parse_sse_fallback(&body, request.id, &self.url, status).await
                    } else {
                        let snippet = if body.len() > 200 {
                            crate::util::truncate_with_ellipsis(&body, 200)
                        } else {
                            body
                        };
                        Err(crate::error::AppError::Mcp(format!(
                            "SSE transport decode error (status={status}, url={}): {json_err} — body: {snippet}",
                            self.url
                        )))
                    }
                }
            }
        }
    }

    async fn send_raw(&mut self, data: &[u8]) -> AppResult<()> {
        let req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(data.to_vec());
        let req = apply_common_headers(req, &self.session_id, &self.headers);

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

        while let Some(event_end) = buffer.find("\n\n") {
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

            let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) else {
                continue;
            };

            if is_notification_or_request(&value) {
                tracing::debug!(
                    "SSE fallback: skipping notification/server-request while awaiting id={request_id}: {value}"
                );
                continue;
            }

            if let Ok(response) = serde_json::from_value::<JsonRpcResponse>(value)
                && let Some(id) = &response.id
                && ids_match(request_id, id)
            {
                return Ok(response);
            }
        }

        Err(crate::error::AppError::Mcp(format!(
            "SSE fallback: no matching response for id={request_id} (status={status}, url={url})"
        )))
    }
}

pub struct DummyTransport;

#[async_trait]
impl Transport for DummyTransport {
    async fn send_request(&mut self, _request: &JsonRpcRequest) -> AppResult<JsonRpcResponse> {
        Err(crate::error::AppError::Mcp(
            "Server is disabled".to_string(),
        ))
    }
    async fn send_raw(&mut self, _data: &[u8]) -> AppResult<()> {
        Err(crate::error::AppError::Mcp(
            "Server is disabled".to_string(),
        ))
    }
    async fn close(&mut self) -> AppResult<()> {
        Ok(())
    }
}
