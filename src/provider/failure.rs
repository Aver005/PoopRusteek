//! Типизированный отказ апстрима: класс, статус, `Retry-After`, заголовки лимитов.
//!
//! Решения принимаются по классу, а не по тексту ошибки. Модуль чистый: ни
//! сети, ни диска, всё проверяется на образцах ответов из документации.

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use std::time::{Duration, SystemTime};

/// Класс отказа по смыслу. Единственный источник правды: шлюз берёт его как есть.
#[derive(Debug, Clone, PartialEq)]
pub enum FailureClass {
    /// Временный лимит частоты: подождать и повторить.
    RateLimit {
        retry_after: Option<Duration>,
    },
    /// Квота или оплата кончились, ожидание секунд не поможет.
    QuotaExhausted {
        resets_at: Option<SystemTime>,
    },
    /// Потолок расходов (Anthropic `billing_error`): долгий перерыв, нужна проба.
    SpendCap,
    /// Сервер перегружен (503/529).
    Overloaded,
    Timeout,
    Server,
    Network,
    /// Ключ неверен или просрочен (401, 403 без признаков блокировки).
    Auth,
    /// Аккаунт заблокирован, приостановлен или лишён доступа.
    AccountBlocked,
    ContextTooLong,
    ModelNotFound,
    ContentFilter,
    /// Запрос неверен сам по себе: на другом аккаунте будет то же.
    BadRequest,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "читает шлюз, он ещё не подключён")
    )]
    Cancelled,
    /// Ответ не разобрался.
    Protocol,
}

impl FailureClass {
    /// Короткий код для журнала и тела ошибок.
    #[expect(dead_code, reason = "читает шлюз, он ещё не подключён")]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RateLimit { .. } => "rate_limit",
            Self::QuotaExhausted { .. } => "quota",
            Self::SpendCap => "spend_cap",
            Self::Overloaded => "overloaded",
            Self::Timeout => "timeout",
            Self::Server => "server_error",
            Self::Network => "network",
            Self::Auth => "auth",
            Self::AccountBlocked => "account_blocked",
            Self::ContextTooLong => "context_too_long",
            Self::ModelNotFound => "model_not_found",
            Self::ContentFilter => "content_filter",
            Self::BadRequest => "bad_request",
            Self::Cancelled => "cancelled",
            Self::Protocol => "protocol",
        }
    }
}

/// Одно окно лимита из заголовков ответа.
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderWindow {
    /// `requests`, `tokens`, `input_tokens`, `output_tokens`.
    pub name: String,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub resets_at: Option<SystemTime>,
}

/// Заголовки лимитов одного ответа. Пусто, если у протокола их нет.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitHeaders {
    pub retry_after: Option<Duration>,
    pub windows: Vec<HeaderWindow>,
    pub observed_at: SystemTime,
}

impl RateLimitHeaders {
    /// Есть ли что сохранять: ни окон, ни `Retry-After` — выбросить.
    pub fn is_empty(&self) -> bool {
        self.retry_after.is_none() && self.windows.is_empty()
    }
}

/// Отказ апстрима с тем, что удалось снять с ответа.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderFailure {
    pub class: FailureClass,
    pub status: Option<StatusCode>,
    pub message: String,
    pub limits: Option<RateLimitHeaders>,
}

impl std::fmt::Display for ProviderFailure {
    /// Тот же текст, что был у `Provider("{status}: {message}")`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "{status}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl ProviderFailure {
    /// Разбор неуспешного HTTP-ответа: статус, заголовки, сырое тело и уже
    /// вынутый текст ошибки.
    pub fn from_response(
        status: StatusCode,
        headers: &HeaderMap,
        body: &str,
        message: String,
        limits: Option<RateLimitHeaders>,
    ) -> Self {
        let retry_after = parse_retry_after(headers, SystemTime::now())
            .or_else(|| limits.as_ref().and_then(|l| l.retry_after))
            .or_else(|| body_retry_delay(body));
        Self {
            class: classify_http(status.as_u16(), retry_after, body, &message),
            status: Some(status),
            message,
            limits,
        }
    }

    /// Ошибка, пришедшая внутри уже открытого потока: статуса нет, класс по тексту.
    pub fn in_stream(message: String) -> Self {
        Self {
            class: classify_stream_error(&message),
            status: None,
            message,
            limits: None,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self.class {
            FailureClass::RateLimit { retry_after } => retry_after,
            _ => self.limits.as_ref().and_then(|l| l.retry_after),
        }
    }
}

// ── Классификация ───────────────────────────────────────────────────────

fn error_field(body: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")?
        .get(key)?
        .as_str()
        .map(str::to_ascii_lowercase)
}

const CONTEXT_HINTS: [&str; 7] = [
    "context_length_exceeded",
    "context length",
    "context window",
    "prompt is too long",
    "maximum context",
    "too many tokens",
    "exceeds the maximum number of tokens",
];

const BLOCK_HINTS: [&str; 7] = [
    "terms of service",
    "violation",
    "suspend",
    "blocked",
    "disabled",
    "on hold",
    "deactivated",
];

/// Класс по статусу и тегам ошибки. Теги (`type`, `code`, `status`) надёжнее
/// кода: OpenAI и Anthropic различают ими квоту и обычный лимит под одним 429.
pub fn classify_http(
    status: u16,
    retry_after: Option<Duration>,
    body: &str,
    message: &str,
) -> FailureClass {
    let tags: Vec<String> = ["type", "code", "status"]
        .iter()
        .filter_map(|key| error_field(body, key))
        .collect();
    let has_tag = |wanted: &[&str]| tags.iter().any(|tag| wanted.contains(&tag.as_str()));
    let text = format!("{} {}", message.to_ascii_lowercase(), tags.join(" "));
    let mentions = |hints: &[&str]| hints.iter().any(|hint| text.contains(hint));

    if has_tag(&["billing_error"]) {
        return FailureClass::SpendCap;
    }
    if has_tag(&[
        "insufficient_quota",
        "billing_not_active",
        "billing_hard_limit_reached",
    ]) {
        return FailureClass::QuotaExhausted { resets_at: None };
    }
    if has_tag(&["content_filter", "content_policy_violation"]) {
        return FailureClass::ContentFilter;
    }
    if has_tag(&["model_not_found"]) {
        return FailureClass::ModelNotFound;
    }
    if has_tag(&["overloaded_error"]) || status == 529 {
        return FailureClass::Overloaded;
    }
    match status {
        401 => FailureClass::Auth,
        402 => FailureClass::QuotaExhausted { resets_at: None },
        403 if mentions(&BLOCK_HINTS) => FailureClass::AccountBlocked,
        403 => FailureClass::Auth,
        404 => FailureClass::ModelNotFound,
        408 | 504 => FailureClass::Timeout,
        413 => FailureClass::ContextTooLong,
        429 => FailureClass::RateLimit { retry_after },
        503 => FailureClass::Overloaded,
        500..=599 => FailureClass::Server,
        _ if mentions(&CONTEXT_HINTS) => FailureClass::ContextTooLong,
        _ => FailureClass::BadRequest,
    }
}

/// Класс ошибки из потока: статуса нет, остаётся текст.
pub fn classify_stream_error(message: &str) -> FailureClass {
    let text = message.to_ascii_lowercase();
    if text.contains("overload") {
        FailureClass::Overloaded
    } else if text.contains("quota") || text.contains("billing") {
        FailureClass::QuotaExhausted { resets_at: None }
    } else if text.contains("rate limit") || text.contains("rate_limit") {
        FailureClass::RateLimit { retry_after: None }
    } else if CONTEXT_HINTS.iter().any(|hint| text.contains(hint)) {
        FailureClass::ContextTooLong
    } else {
        FailureClass::Server
    }
}

// ── Заголовки ───────────────────────────────────────────────────────────

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_str(headers, name)?.parse().ok()
}

/// `Retry-After`: целые секунды или HTTP-дата.
pub fn parse_retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = header_str(headers, "retry-after")?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let target: SystemTime = date.with_timezone(&chrono::Utc).into();
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Gemini кладёт задержку в тело: `details[].retryDelay = "34s"`.
fn body_retry_delay(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")?
        .get("details")?
        .as_array()?
        .iter()
        .find_map(|detail| detail.get("retryDelay")?.as_str())
        .and_then(parse_go_duration)
}

/// Длительность вида `1s`, `6m0s`, `20ms`, `1h2m3.5s` (формат OpenAI и Google).
pub fn parse_go_duration(text: &str) -> Option<Duration> {
    let mut total = 0f64;
    let mut number = String::new();
    let mut parsed_any = false;
    let mut unit = String::new();
    let flush = |number: &mut String, unit: &mut String, total: &mut f64| -> Option<()> {
        if number.is_empty() {
            return Some(());
        }
        let value: f64 = number.parse().ok()?;
        let seconds = match unit.as_str() {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        *total += value * seconds;
        number.clear();
        unit.clear();
        Some(())
    };
    for ch in text.trim().chars() {
        if ch.is_ascii_digit() || ch == '.' {
            if !unit.is_empty() {
                flush(&mut number, &mut unit, &mut total)?;
            }
            number.push(ch);
        } else {
            unit.push(ch);
        }
    }
    parsed_any |= !number.is_empty();
    flush(&mut number, &mut unit, &mut total)?;
    (parsed_any && total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

/// OpenAI: `x-ratelimit-{limit,remaining,reset}-{requests,tokens}`.
pub fn parse_openai_limits(headers: &HeaderMap, now: SystemTime) -> RateLimitHeaders {
    let windows = ["requests", "tokens"]
        .iter()
        .filter_map(|name| {
            let limit = header_u64(headers, &format!("x-ratelimit-limit-{name}"));
            let remaining = header_u64(headers, &format!("x-ratelimit-remaining-{name}"));
            let resets_at = header_str(headers, &format!("x-ratelimit-reset-{name}"))
                .and_then(parse_go_duration)
                .map(|after| now + after);
            (limit.is_some() || remaining.is_some() || resets_at.is_some()).then(|| HeaderWindow {
                name: (*name).to_string(),
                limit,
                remaining,
                resets_at,
            })
        })
        .collect();
    RateLimitHeaders {
        retry_after: parse_retry_after(headers, now),
        windows,
        observed_at: now,
    }
}

/// Anthropic: `anthropic-ratelimit-<окно>-{limit,remaining,reset}`, `reset` — RFC 3339.
pub fn parse_anthropic_limits(headers: &HeaderMap, now: SystemTime) -> RateLimitHeaders {
    let windows = ["requests", "tokens", "input-tokens", "output-tokens"]
        .iter()
        .filter_map(|name| {
            let limit = header_u64(headers, &format!("anthropic-ratelimit-{name}-limit"));
            let remaining = header_u64(headers, &format!("anthropic-ratelimit-{name}-remaining"));
            let resets_at = header_str(headers, &format!("anthropic-ratelimit-{name}-reset"))
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|date| SystemTime::from(date.with_timezone(&chrono::Utc)));
            (limit.is_some() || remaining.is_some() || resets_at.is_some()).then(|| HeaderWindow {
                name: name.replace('-', "_"),
                limit,
                remaining,
                resets_at,
            })
        })
        .collect();
    RateLimitHeaders {
        retry_after: parse_retry_after(headers, now),
        windows,
        observed_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn failure(status: u16, pairs: &[(&str, &str)], body: &str) -> ProviderFailure {
        ProviderFailure::from_response(
            StatusCode::from_u16(status).unwrap(),
            &headers(pairs),
            body,
            "msg".to_string(),
            None,
        )
    }

    #[test]
    fn a_429_with_retry_after_carries_the_delay() {
        let f = failure(429, &[("retry-after", "42")], "{}");
        assert_eq!(
            f.class,
            FailureClass::RateLimit {
                retry_after: Some(Duration::from_secs(42))
            }
        );
    }

    #[test]
    fn a_429_without_retry_after_is_a_plain_rate_limit() {
        let f = failure(
            429,
            &[],
            r#"{"error":{"type":"rate_limit_error","message":"slow"}}"#,
        );
        assert_eq!(f.class, FailureClass::RateLimit { retry_after: None });
    }

    #[test]
    fn retry_after_may_be_an_http_date_in_the_past() {
        let h = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]);
        assert_eq!(
            parse_retry_after(&h, SystemTime::now()),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn quota_and_spend_cap_are_not_rate_limits() {
        let quota = failure(
            429,
            &[],
            r#"{"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"x"}}"#,
        );
        assert!(matches!(quota.class, FailureClass::QuotaExhausted { .. }));
        let cap = failure(
            429,
            &[],
            r#"{"type":"error","error":{"type":"billing_error","message":"spend limit"}}"#,
        );
        assert_eq!(cap.class, FailureClass::SpendCap);
    }

    #[test]
    fn gemini_puts_the_delay_in_the_body() {
        let body = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[{"retryDelay":"34s"}]}}"#;
        let f = failure(429, &[], body);
        assert_eq!(
            f.class,
            FailureClass::RateLimit {
                retry_after: Some(Duration::from_secs(34))
            }
        );
    }

    #[test]
    fn statuses_map_to_classes() {
        assert_eq!(failure(401, &[], "").class, FailureClass::Auth);
        assert_eq!(failure(403, &[], "").class, FailureClass::Auth);
        let blocked = ProviderFailure::from_response(
            StatusCode::FORBIDDEN,
            &HeaderMap::new(),
            "",
            "Your account has been suspended".to_string(),
            None,
        );
        assert_eq!(blocked.class, FailureClass::AccountBlocked);
        assert_eq!(failure(404, &[], "").class, FailureClass::ModelNotFound);
        assert_eq!(failure(500, &[], "").class, FailureClass::Server);
        assert_eq!(failure(503, &[], "").class, FailureClass::Overloaded);
        assert_eq!(failure(529, &[], "").class, FailureClass::Overloaded);
        assert_eq!(failure(504, &[], "").class, FailureClass::Timeout);
        assert_eq!(failure(400, &[], "").class, FailureClass::BadRequest);
    }

    #[test]
    fn a_400_about_the_context_window_is_context_too_long() {
        let f = ProviderFailure::from_response(
            StatusCode::BAD_REQUEST,
            &HeaderMap::new(),
            r#"{"error":{"code":"context_length_exceeded","message":"x"}}"#,
            "This model's maximum context length is 8192 tokens".to_string(),
            None,
        );
        assert_eq!(f.class, FailureClass::ContextTooLong);
    }

    #[test]
    fn an_in_stream_error_is_classified_by_its_text() {
        assert_eq!(
            ProviderFailure::in_stream("Overloaded".to_string()).class,
            FailureClass::Overloaded
        );
        assert_eq!(
            ProviderFailure::in_stream("boom".to_string()).class,
            FailureClass::Server
        );
    }

    #[test]
    fn go_durations_parse() {
        assert_eq!(parse_go_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_go_duration("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(parse_go_duration("20ms"), Some(Duration::from_millis(20)));
        assert_eq!(
            parse_go_duration("1h2m3.5s"),
            Some(Duration::from_secs_f64(3723.5))
        );
        assert_eq!(parse_go_duration("soon"), None);
    }

    #[test]
    fn openai_headers_become_windows() {
        let h = headers(&[
            ("x-ratelimit-limit-requests", "60"),
            ("x-ratelimit-remaining-requests", "59"),
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-limit-tokens", "150000"),
            ("x-ratelimit-remaining-tokens", "149984"),
            ("x-ratelimit-reset-tokens", "6m0s"),
        ]);
        let now = SystemTime::now();
        let parsed = parse_openai_limits(&h, now);
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].remaining, Some(59));
        assert_eq!(
            parsed.windows[1].resets_at,
            Some(now + Duration::from_secs(360))
        );
        assert!(parse_openai_limits(&HeaderMap::new(), now).is_empty());
    }

    #[test]
    fn anthropic_headers_become_windows() {
        let h = headers(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "49"),
            ("anthropic-ratelimit-requests-reset", "2026-11-06T12:00:00Z"),
            ("anthropic-ratelimit-input-tokens-remaining", "1000"),
            ("retry-after", "7"),
        ]);
        let parsed = parse_anthropic_limits(&h, SystemTime::now());
        assert_eq!(parsed.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[1].name, "input_tokens");
        assert!(parsed.windows[0].resets_at.is_some());
    }

    #[test]
    fn display_keeps_the_old_provider_error_text() {
        let f = failure(429, &[], "{}");
        assert_eq!(f.to_string(), "429 Too Many Requests: msg");
    }
}
