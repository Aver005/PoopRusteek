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
use tokio::time::{Instant, sleep};

const DEEPSEEK_HOST: &str = "chat.deepseek.com";

/// Ответ — HTML-страница: так WAF/капча отвечают вместо API, иногда с 200 OK.
pub(super) fn is_html_response(response: &Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/html"))
}

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
        return Some(ApiRefusal::Transport(super::client::with_hint(
            format!("{message} (code {code})"),
            Some(code),
        )));
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

/// Ответ помечен как JSON. Для потоковых эндпоинтов это значит отказ:
/// поток приходит как `text/event-stream`.
pub(super) fn is_json_response(response: &Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
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

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36";

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
        super::client::insert_identity(&mut headers, &self.client_version);
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
        if key.eq_ignore_ascii_case("x-ds-pow-response") {
            return format!("<base64:{} chars>", value.len());
        }
        // Authorization, cookie и родня скрыты целиком: префикс и хвост ключа — тоже ключ.
        crate::secrets::redact::header_value(key, value)
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

    /// Подождать своей очереди у общего для всех форков ограничителя.
    pub(super) async fn enforce_rate_limit(&self) {
        self.limiter
            .wait(self.rate_limit_ms, self.rate_limit_per_minute)
            .await;
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
    async fn back_off(
        action: &str,
        attempt: &mut usize,
        max_attempts: usize,
        reason: &str,
        wait: Duration,
    ) {
        *attempt += 1;
        let n = *attempt;
        tracing::warn!("{action} {reason}, retry {n}/{max_attempts} in {wait:?}");
        debug_log::log(
            action,
            format!("{reason}; retry {n} in {}ms", wait.as_millis()),
        );
        sleep(wait).await;
    }

    pub(super) async fn send_json_request(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
        body: &Value,
        resend: Resend,
    ) -> AppResult<Response> {
        self.send_with_retry(action, url, headers, Some(body), resend)
            .await
    }

    /// Один запрос с повторами по политике `resend`; ограничитель — на каждой
    /// попытке. 429 у безопасных запросов ждёт `Retry-After` и повторяется.
    async fn send_with_retry(
        &self,
        action: &str,
        url: &str,
        headers: &HeaderMap,
        body: Option<&Value>,
        resend: Resend,
    ) -> AppResult<Response> {
        let max_attempts = attempts_for(self.max_retries);
        let mut attempt = 0;
        loop {
            self.enforce_rate_limit().await;
            self.log_http_request(action, url, headers, body.unwrap_or(&Value::Null));
            let request = match body {
                Some(body) => self.client.post(url).json(body),
                None => self.client.get(url),
            };
            let last_try = attempt + 1 >= max_attempts;
            match request.headers(headers.clone()).send().await {
                Ok(response) => {
                    let status = response.status();
                    debug_log::log(
                        action,
                        format!(
                            "response status={status} headers={}",
                            Self::headers_to_debug_json(response.headers())
                        ),
                    );
                    let retry = match status {
                        // Отправку с PoW повторяет вызывающий, с новым решением:
                        // заголовок мог протухнуть, пока ждали `Retry-After`.
                        reqwest::StatusCode::TOO_MANY_REQUESTS => resend == Resend::Safe,
                        // 5xx от CloudFront бывает и после того, как сервер
                        // принял сообщение: повтор задвоил бы его в сессии.
                        _ if status.is_server_error() => resend == Resend::Safe,
                        _ => false,
                    };
                    if !retry || last_try {
                        return Ok(response);
                    }
                    let wait =
                        retry_after(&response).unwrap_or_else(|| Self::retry_backoff(attempt + 1));
                    Self::back_off(
                        action,
                        &mut attempt,
                        max_attempts,
                        &format!("status {status}"),
                        wait,
                    )
                    .await;
                }
                Err(error) => {
                    // Без соединения запрос точно не ушёл — повтор безопасен
                    // всегда. Иначе — только для запросов без последствий.
                    let retry = error.is_connect() || resend == Resend::Safe;
                    if !retry || last_try {
                        debug_log::log(
                            action,
                            format!("request failed before HTTP response: {error}"),
                        );
                        return Err(AppError::Http(error));
                    }
                    let wait = Self::retry_backoff(attempt + 1);
                    Self::back_off(
                        action,
                        &mut attempt,
                        max_attempts,
                        &format!("connection error: {error}"),
                        wait,
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
        let html = is_html_response(&response);
        let text = response.text().await.unwrap_or_default();
        debug_log::log(
            action,
            format!("response error status={status} body={text}"),
        );
        // Тело ошибки бывает страницей WAF в десятки КБ — в сообщение идёт суть.
        let detail = if html {
            "an HTML page (likely a WAF or captcha check — open chat.deepseek.com in the browser and log in again)".to_string()
        } else {
            crate::util::truncate_with_ellipsis(text.trim(), 300)
        };
        let message = format!("{label}: {status} {detail}");
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            AppError::RateLimited(message)
        } else {
            AppError::Provider(message)
        }
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
        self.send_with_retry(action, url, headers, None, Resend::Safe)
            .await
    }
}

/// Можно ли повторить запрос, если ответ не дошёл или сервер упал.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resend {
    /// Повтор ничего не меняет на сервере (список, проверка, PoW).
    Safe,
    /// Повтор задвоил бы сообщение или сессию: только при ошибке соединения.
    ConnectOnly,
}

/// Сколько попыток даёт `max_retries`: `-1` — без предела, прочее
/// отрицательное — опечатка, а не «бесконечно».
pub(super) fn attempts_for(max_retries: i32) -> usize {
    match max_retries {
        -1 => usize::MAX,
        n if n < 0 => 1,
        n => n as usize + 1,
    }
}

/// `Retry-After` в секундах, не дольше минуты.
fn retry_after(response: &Response) -> Option<Duration> {
    let seconds: u64 = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds.min(60)))
}

/// Общий для провайдера и всех его форков ограничитель частоты: лимит у
/// DeepSeek на аккаунт, а не на чат.
#[derive(Debug, Default)]
pub(super) struct RateLimiter {
    next_slot: std::sync::Mutex<Option<Instant>>,
    history: std::sync::Mutex<std::collections::VecDeque<Instant>>,
}

impl RateLimiter {
    /// Два независимых условия: шаг между запросами (`spacing_ms`) и потолок
    /// за скользящую минуту (`per_minute`). Ноль выключает условие.
    async fn wait(&self, spacing_ms: u64, per_minute: u32) {
        if spacing_ms > 0 {
            // Слот занимается под замком: параллельные форки встают в очередь,
            // а не просыпаются разом от одного и того же «последнего» запроса.
            let spacing = Duration::from_millis(spacing_ms);
            let slot = self.next_slot.lock().ok().map(|mut next| {
                let at = next.map_or_else(Instant::now, |at| at.max(Instant::now()));
                *next = Some(at + spacing);
                at
            });
            if let Some(at) = slot {
                tokio::time::sleep_until(at).await;
            }
        }

        if per_minute == 0 {
            return;
        }
        let window = Duration::from_secs(60);
        loop {
            let wait = {
                // Отравленный замок — без ограничения, а не паника всего TUI.
                let Ok(mut history) = self.history.lock() else {
                    return;
                };
                let now = Instant::now();
                while let Some(&oldest) = history.front() {
                    if now.duration_since(oldest) >= window {
                        history.pop_front();
                    } else {
                        break;
                    }
                }
                if history.len() < per_minute as usize {
                    history.push_back(now);
                    return;
                }
                window - now.duration_since(history[0])
            };
            sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApiRefusal, RateLimiter, api_refusal, attempts_for, is_json_body, not_json_message,
    };

    /// Параллельные запросы встают в очередь: третий ждёт два шага, а не один.
    #[tokio::test]
    async fn concurrent_waits_are_spaced_one_after_another() {
        let limiter = RateLimiter::default();
        let started = tokio::time::Instant::now();
        tokio::join!(
            limiter.wait(60, 0),
            limiter.wait(60, 0),
            limiter.wait(60, 0)
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(120));
    }

    /// Первый запрос не ждёт: шаг считается от предыдущего, а его нет.
    #[tokio::test]
    async fn first_wait_is_immediate() {
        let limiter = RateLimiter::default();
        let started = tokio::time::Instant::now();
        limiter.wait(5_000, 0).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(1_000));
    }

    /// `-1` — без предела; прочее отрицательное — опечатка, а не бесконечность.
    #[test]
    fn max_retries_maps_to_attempts() {
        assert_eq!(attempts_for(-1), usize::MAX);
        assert_eq!(attempts_for(-5), 1);
        assert_eq!(attempts_for(0), 1);
        assert_eq!(attempts_for(3), 4);
    }

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
