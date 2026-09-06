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

/// Текст про «ответ не JSON». Отдельная функция, потому что это и есть весь
/// смысл проверки — объяснить следующему человеку, что путь снесли; а склейка
/// из многострочного литерала уже один раз приехала с дырами из пробелов.
fn not_json_message(label: &str, content_type: &str, bytes: usize) -> String {
    let kind = if content_type.is_empty() {
        "an unlabelled body"
    } else {
        content_type
    };
    format!(
        "{label}: the endpoint answered {kind} in {bytes} bytes instead of JSON. A removed API path is served the site shell with 200 OK, not a 404 — the path is probably gone"
    )
}

/// Отказ, приехавший внутри `200 OK`. Два кода, и означают они разное — от
/// того, какой сработал, зависит, вправе ли вызывающий что-то стирать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ApiRefusal {
    /// Внешний `code`: до дела не дошло — протухший токен, лимит, метод.
    /// Про сам запрошенный объект это не говорит ничего.
    Transport(String),
    /// `data.biz_code`: API ответил **про запрошенное** и отказал. Именно так
    /// выглядит несуществующая сессия: `code: 0`, `biz_code: 1`,
    /// `biz_msg: "invalid chat session id"`, `biz_data: null`.
    Business(String),
}

impl ApiRefusal {
    pub(super) fn message(&self) -> &str {
        match self {
            Self::Transport(message) | Self::Business(message) => message,
        }
    }
}

/// Отказ из конверта `{code, msg, data: {biz_code, biz_msg, biz_data}}`.
///
/// Смотреть **оба** кода обязательно: внешний остаётся нулевым, когда отказ
/// деловой, и проверка только по нему принимает «такой сессии нет» за успех
/// с пустыми данными.
pub(super) fn api_refusal(payload: &Value) -> Option<ApiRefusal> {
    let text = |value: &Value, fallback: &str| {
        let message = value.as_str().unwrap_or_default().trim().to_string();
        if message.is_empty() {
            fallback.to_string()
        } else {
            message
        }
    };
    if let Some(code) = payload["code"].as_i64()
        && code != 0
    {
        let message = text(&payload["msg"], "no message");
        return Some(ApiRefusal::Transport(format!("{message} (code {code})")));
    }
    if let Some(biz_code) = payload["data"]["biz_code"].as_i64()
        && biz_code != 0
    {
        let message = text(&payload["data"]["biz_msg"], "no message");
        return Some(ApiRefusal::Business(format!(
            "{message} (biz_code {biz_code})"
        )));
    }
    None
}

/// Похоже ли тело на JSON. Заголовок — первый довод, но не единственный:
/// он бывает пустым или обобщённым, а вот HTML-оболочка сайта не начинается
/// ни с `{`, ни с `[` никогда.
fn is_json_body(content_type: &str, body: &str) -> bool {
    let lowered = content_type.to_ascii_lowercase();
    if lowered.contains("json") {
        return true;
    }
    if lowered.contains("html") || lowered.contains("text/plain") {
        return false;
    }
    matches!(body.trim_start().as_bytes().first(), Some(b'{' | b'['))
}

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

    /// Отложить следующую попытку: счётчик вперёд, отступ от нового счётчика,
    /// строка в журнал. Четыре ветки повтора делали это порознь, а порядок
    /// «сначала счётчик, потом отступ» здесь важен.
    async fn back_off(action: &str, attempt: &mut usize, max_attempts: usize, reason: &str) {
        *attempt += 1;
        let n = *attempt;
        let capped = Self::retry_backoff(n);
        tracing::warn!("{action} {reason}, retry {n}/{max_attempts} in {capped:?}");
        sleep(capped).await;
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

                    Self::back_off(
                        action,
                        &mut attempt,
                        max_attempts,
                        &format!("server error {status}"),
                    )
                    .await;
                }
                Err(error) => {
                    if attempt + 1 >= max_attempts {
                        debug_log::log(
                            action,
                            format!("request failed before HTTP response: {error}"),
                        );
                        return Err(AppError::Http(error));
                    }
                    Self::back_off(
                        action,
                        &mut attempt,
                        max_attempts,
                        &format!("connection error: {error}"),
                    )
                    .await;
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

    /// Прочитать тело ответа как JSON, отличая «эндпоинта больше нет» от
    /// «разбор сломался».
    ///
    /// Мёртвый путь под CloudFront отвечает не 404, а **200 OK и HTML-обо**
    /// лочкой сайта, поэтому `response.json()` падал ошибкой serde про
    /// неожиданный `<` в первой позиции — по ней невозможно догадаться, что
    /// endpoint просто снесли. Это стоило одного дня разматывания: сессия
    /// считалась мёртвой через семь секунд после создания.
    pub(super) async fn read_json<T: serde::de::DeserializeOwned>(
        action: &str,
        response: Response,
        label: &str,
    ) -> AppResult<T> {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = response.text().await.map_err(AppError::Http)?;
        if !is_json_body(&content_type, &text) {
            debug_log::log(
                action,
                format!(
                    "response was not JSON: content-type={content_type} bytes={} body={}",
                    text.len(),
                    crate::util::truncate_at_char_boundary(&text, 400)
                ),
            );
            return Err(AppError::Provider(not_json_message(
                label,
                &content_type,
                text.len(),
            )));
        }
        serde_json::from_str(&text).map_err(|error| {
            debug_log::log(
                action,
                format!(
                    "response JSON did not fit the expected shape: {error} body={}",
                    crate::util::truncate_at_char_boundary(&text, 400)
                ),
            );
            AppError::Provider(format!("{label}: unexpected response shape: {error}"))
        })
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

                    Self::back_off(
                        action,
                        &mut attempt,
                        max_attempts,
                        &format!("server error {status}"),
                    )
                    .await;
                }
                Err(error) => {
                    if attempt + 1 >= max_attempts {
                        return Err(AppError::Http(error));
                    }
                    Self::back_off(
                        action,
                        &mut attempt,
                        max_attempts,
                        &format!("connection error: {error}"),
                    )
                    .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiRefusal, api_refusal, is_json_body, not_json_message};

    /// Текст объяснения — единственная польза этой проверки, и он уже однажды
    /// приехал с провалами по 18 пробелов из многострочного литерала.
    #[test]
    fn the_explanation_reads_as_one_clean_sentence() {
        let message = not_json_message("Session history failed", "text/html; charset=utf-8", 10071);
        assert!(!message.contains("  "), "double spaces in: {message}");
        assert!(!message.contains('\n'), "newlines in: {message}");
        assert!(message.contains("text/html"), "{message}");
        assert!(message.contains("10071 bytes"), "{message}");
        assert!(message.contains("probably gone"), "{message}");
        // Без заголовка тип не выдумывается.
        assert!(
            not_json_message("x", "", 12).contains("an unlabelled body"),
            "{message}"
        );
    }

    /// Ровно та ловушка, ради которой проверка появилась: снесённый путь
    /// отвечает 200 OK и оболочкой сайта, а не 404.
    #[test]
    fn the_site_shell_is_not_mistaken_for_json() {
        assert!(!is_json_body(
            "text/html; charset=utf-8",
            "<!doctype html><html><head>"
        ));
        assert!(!is_json_body("text/plain", "gateway timeout"));
        // Заголовка нет — решает первый символ тела.
        assert!(!is_json_body("", "<!doctype html>"));
    }

    /// Ровно тот ответ, которым API встречает несуществующую сессию:
    /// HTTP 200, внешний `code: 0`, отказ — во внутреннем `biz_code`.
    /// Проверка только внешнего кода принимала его за успех, сессия
    /// «подхватывалась», и ход падал пустыми ответами.
    #[test]
    fn a_refusal_inside_a_200_is_seen_in_the_inner_code() {
        let payload = serde_json::json!({
            "code": 0,
            "msg": "",
            "data": { "biz_code": 1, "biz_msg": "invalid chat session id", "biz_data": null }
        });
        let refusal = api_refusal(&payload).expect("an inner refusal must be seen");
        assert!(
            matches!(refusal, ApiRefusal::Business(_)),
            "a refusal about the requested object is a business one: {refusal:?}"
        );
        assert!(
            refusal.message().contains("invalid chat session id"),
            "{refusal:?}"
        );
        assert!(refusal.message().contains("biz_code 1"), "{refusal:?}");
    }

    /// Внешний код — не про запрошенный объект, а про сам запрос: протухший
    /// токен ничего не сообщает о судьбе сессии.
    #[test]
    fn an_outer_code_is_transport_not_business() {
        let payload = serde_json::json!({
            "code": 40003,
            "msg": "Authorization Failed (invalid token)",
            "data": null
        });
        let refusal = api_refusal(&payload).expect("an outer refusal must be seen");
        assert!(matches!(refusal, ApiRefusal::Transport(_)), "{refusal:?}");
        assert!(refusal.message().contains("40003"), "{refusal:?}");
    }

    #[test]
    fn a_healthy_envelope_carries_no_refusal() {
        let payload = serde_json::json!({
            "code": 0,
            "msg": "",
            "data": { "biz_code": 0, "biz_msg": "", "biz_data": { "chat_messages": [] } }
        });
        assert!(api_refusal(&payload).is_none());
        // Конверта нет вовсе (не-DeepSeek форма) — отказом это не считается.
        assert!(api_refusal(&serde_json::json!({ "anything": 1 })).is_none());
    }

    #[test]
    fn json_is_recognised_by_header_or_by_body() {
        assert!(is_json_body("application/json", "{\"code\":0}"));
        assert!(is_json_body("application/json; charset=utf-8", ""));
        assert!(is_json_body("", "  {\"code\":0}"));
        assert!(is_json_body("", "[1,2]"));
    }
}
