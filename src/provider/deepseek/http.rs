//! Low-level HTTP transport plumbing shared by every DeepSeek endpoint:
//! auth headers, request/response debug logging (with secret redaction),
//! retry backoff, rate limiting, and the generic JSON/GET request senders
//! that all the endpoint wrappers build on.

use super::DeepseekProvider;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use reqwest::{
    Response,
    header::{HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;

const DEEPSEEK_HOST: &str = "chat.deepseek.com";

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 YaBrowser/26.3.0.0 Safari/537.36";

impl DeepseekProvider {
    pub(super) fn auth_headers(&self) -> AppResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("Host", HeaderValue::from_static(DEEPSEEK_HOST));
        headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        // No manual Accept-Encoding: setting it by hand disables reqwest's
        // auto-decompression, and gzip isn't among our enabled features — a
        // server honoring it would hand us bytes we'd garble.
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("x-client-platform", HeaderValue::from_static("android"));
        headers.insert("x-client-version", HeaderValue::from_static("1.8.0"));
        headers.insert("x-client-locale", HeaderValue::from_static("zh_CN"));
        headers.insert("accept-charset", HeaderValue::from_static("UTF-8"));
        let bearer = format!("Bearer {}", self.token);
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&bearer)
                .map_err(|e| AppError::Provider(format!("Invalid auth header: {e}")))?,
        );
        Ok(headers)
    }

    pub(super) fn redact_value(key: &str, value: &str) -> String {
        let lower = key.to_ascii_lowercase();
        if lower == "authorization" {
            if value.len() > 24 {
                let head = crate::util::truncate_at_char_boundary(value, 16);
                let tail_start = value.len().saturating_sub(8);
                let tail_start = if value.is_char_boundary(tail_start) {
                    tail_start
                } else {
                    let mut i = tail_start;
                    while i < value.len() && !value.is_char_boundary(i) {
                        i += 1;
                    }
                    i
                };
                return format!("{}...{}", head, &value[tail_start..]);
            }
            return "<redacted>".to_string();
        }
        if lower == "x-ds-pow-response" {
            return format!("<base64:{} chars>", value.len());
        }
        value.to_string()
    }

    pub(super) fn headers_to_debug_json(headers: &HeaderMap) -> Value {
        let mut map = serde_json::Map::new();
        for (key, value) in headers {
            let raw = value
                .to_str()
                .map(|text| Self::redact_value(key.as_str(), text))
                .unwrap_or_else(|_| "<binary>".to_string());
            map.insert(key.to_string(), Value::String(raw));
        }
        Value::Object(map)
    }

    pub(super) fn log_http_request(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
    ) {
        debug_log::log_json(
            action,
            &json!({
                "url": url,
                "headers": Self::headers_to_debug_json(headers),
                "body": body,
            }),
        );
    }

    /// Applies two independent, composable gates before letting a request
    /// through: a minimum spacing between consecutive requests
    /// (`rate_limit_ms`) and a cap on how many requests may fire within any
    /// rolling 60s window (`rate_limit_per_minute`). Either can be 0 to
    /// disable it; both can be active at once.
    pub(super) async fn enforce_rate_limit(&self) {
        if self.rate_limit_ms > 0 {
            let elapsed = self
                .last_request
                .lock()
                .map(|last| last.elapsed())
                .unwrap_or(Duration::from_secs(60));
            let min_interval = Duration::from_millis(self.rate_limit_ms);
            if elapsed < min_interval {
                sleep(min_interval - elapsed).await;
            }
        }
        let _ = self
            .last_request
            .lock()
            .map(|mut last| *last = std::time::Instant::now());

        if self.rate_limit_per_minute > 0 {
            let window = Duration::from_secs(60);
            loop {
                let wait = {
                    // Poison-safe like every other lock in the provider: a
                    // panicked writer must degrade to "skip rate limiting",
                    // not take down the whole TUI.
                    let Ok(mut history) = self.request_history.lock() else {
                        break;
                    };
                    let now = std::time::Instant::now();
                    while let Some(&oldest) = history.front() {
                        if now.duration_since(oldest) >= window {
                            history.pop_front();
                        } else {
                            break;
                        }
                    }
                    if history.len() < self.rate_limit_per_minute as usize {
                        history.push_back(now);
                        None
                    } else {
                        Some(window - now.duration_since(history[0]))
                    }
                };
                match wait {
                    None => break,
                    Some(duration) => sleep(duration).await,
                }
            }
        }
    }

    /// Exponential backoff for retry loops: 1s, 2s, 4s… capped at 30s.
    /// Saturating on purpose — infinite-retry mode (`max_retries = -1`) reaches
    /// attempt counts where `2u64.pow(attempt)` would overflow-panic.
    pub(super) fn retry_backoff(attempt: usize) -> Duration {
        let exp = attempt.saturating_sub(1).min(5) as u32;
        Duration::from_millis(1000u64 << exp).min(Duration::from_secs(30))
    }

    pub(super) async fn send_json_request(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
    ) -> AppResult<Response> {
        self.enforce_rate_limit().await;

        let max_attempts = match self.max_retries {
            -1 => usize::MAX,
            0 => 1,
            n => (n as usize) + 1,
        };

        let mut attempt = 0;
        loop {
            self.log_http_request(action, url, headers, body);

            match self
                .client
                .post(url)
                .headers(headers.clone())
                .json(body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    debug_log::log(
                        action,
                        format!(
                            "response status={status} headers={}",
                            Self::headers_to_debug_json(response.headers())
                        ),
                    );

                    if !status.is_server_error() || attempt + 1 >= max_attempts {
                        return Ok(response);
                    }

                    attempt += 1;
                    let capped = Self::retry_backoff(attempt);
                    tracing::warn!(
                        "{action} server error {status}, retry {attempt}/{max_attempts} in {capped:?}"
                    );
                    sleep(capped).await;
                }
                Err(error) => {
                    if attempt + 1 >= max_attempts {
                        debug_log::log(
                            action,
                            format!("request failed before HTTP response: {error}"),
                        );
                        return Err(AppError::Http(error));
                    }
                    attempt += 1;
                    let capped = Self::retry_backoff(attempt);
                    tracing::warn!(
                        "{action} connection error: {error}, retry {attempt}/{max_attempts} in {capped:?}"
                    );
                    sleep(capped).await;
                }
            }
        }
    }

    pub(super) async fn read_error_response(
        action: &str,
        response: Response,
        label: &str,
    ) -> AppError {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        debug_log::log(
            action,
            format!("response error status={status} body={text}"),
        );
        AppError::Provider(format!("{label}: {status} {text}"))
    }

    // ─── Generic GET request helper ────────────────────────────

    pub(super) async fn send_get_request(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
    ) -> AppResult<Response> {
        self.enforce_rate_limit().await;

        let max_attempts = match self.max_retries {
            -1 => usize::MAX,
            0 => 1,
            n => (n as usize) + 1,
        };

        let mut attempt = 0;
        loop {
            self.log_http_request(action, url, headers, &Value::Null);

            match self.client.get(url).headers(headers.clone()).send().await {
                Ok(response) => {
                    let status = response.status();
                    debug_log::log(action, format!("response status={status}"));

                    if !status.is_server_error() || attempt + 1 >= max_attempts {
                        return Ok(response);
                    }

                    attempt += 1;
                    let capped = Self::retry_backoff(attempt);
                    tracing::warn!(
                        "{action} server error {status}, retry {attempt}/{max_attempts} in {capped:?}"
                    );
                    sleep(capped).await;
                }
                Err(error) => {
                    if attempt + 1 >= max_attempts {
                        return Err(AppError::Http(error));
                    }
                    attempt += 1;
                    let capped = Self::retry_backoff(attempt);
                    tracing::warn!(
                        "{action} connection error: {error}, retry {attempt}/{max_attempts} in {capped:?}"
                    );
                    sleep(capped).await;
                }
            }
        }
    }
}
