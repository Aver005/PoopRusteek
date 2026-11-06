//! `/serve` поверх шлюза: живой дескриптор, каталог моделей, проверка адреса,
//! заголовки `x-pooprusteek-*` и статус. Чистые функции над снимком, без сети.

use super::catalog::FetchedModels;
use crate::gateway::handle::{
    AccountStatus, AccountView, Decision, DisabledGateway, GatewayHandle, GatewaySnapshot,
    LimitSnapshot, LimitSource, ServedBy, Why, WindowKind,
};
use crate::gateway::types::{AccountId, AccountProvider, Route, RouteId};
use crate::provider::deepseek::mode::{DeepseekMode, LISTED_MODELS as DEEPSEEK_MODELS};
use hyper::header::{HeaderName, HeaderValue};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

pub const HEADER_ACCOUNT: &str = "x-pooprusteek-account";
pub const HEADER_MODEL: &str = "x-pooprusteek-model";
pub const HEADER_ROUTE: &str = "x-pooprusteek-route";
pub const HEADER_FALLBACK: &str = "x-pooprusteek-fallback";
pub const HEADER_FALLBACK_REASON: &str = "x-pooprusteek-fallback-reason";

/// Живой шлюз сервера: приложение подменяет его при пересборке, сервер читает на каждом запросе.
#[derive(Clone)]
pub struct GatewayCell(Arc<RwLock<GatewayHandle>>);

impl GatewayCell {
    pub fn new(gateway: GatewayHandle) -> Self {
        Self(Arc::new(RwLock::new(gateway)))
    }

    pub fn set(&self, gateway: GatewayHandle) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = gateway;
    }

    pub fn get(&self) -> GatewayHandle {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Default for GatewayCell {
    /// Без шлюза: прежний путь `/serve`.
    fn default() -> Self {
        Self::new(Arc::new(DisabledGateway::new()))
    }
}

impl std::fmt::Debug for GatewayCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GatewayCell")
    }
}

// ── Каталог ─────────────────────────────────────────────────────────────

/// Отдаётся ли аккаунт наружу: не подписка CLI, включён, адаптер собран.
fn is_served(account: &AccountView) -> bool {
    account.provider.cli_tool().is_none()
        && account.enabled
        && !matches!(account.status, AccountStatus::Unavailable { .. })
}

/// Модели аккаунта, которые видно снаружи: по умолчанию, избранные, найденные у апстрима.
fn account_models(account: &AccountView, fetched: &FetchedModels) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    if account.provider == AccountProvider::DeepseekWeb {
        models.extend(DEEPSEEK_MODELS.iter().map(|m| m.to_string()));
    }
    let listed = account
        .default_model
        .iter()
        .chain(&account.favorites)
        .chain(fetched.get(account.id.as_str()).into_iter().flatten());
    for model in listed {
        if !model.is_empty() && !models.contains(model) {
            models.push(model.clone());
        }
    }
    models
}

fn route_is_served(route: &Route, snapshot: &GatewaySnapshot) -> bool {
    route.steps.iter().any(|step| {
        snapshot
            .accounts
            .iter()
            .any(|account| account.id == step.account && is_served(account))
    })
}

/// Всё, что `GET /v1/models` объявляет: `<account>/<model>` и `auto/<route>`.
pub fn model_ids(snapshot: &GatewaySnapshot, fetched: &FetchedModels) -> Vec<String> {
    let mut ids = Vec::new();
    for route in &snapshot.routes {
        if route_is_served(route, snapshot) {
            ids.push(format!("auto/{}", route.id));
        }
    }
    for account in snapshot.accounts.iter().filter(|a| is_served(a)) {
        for model in account_models(account, fetched) {
            ids.push(format!("{}/{model}", account.id));
        }
    }
    ids
}

/// Допустимые в адресе символы: ни пробелов, ни управляющих, ни путей и запросов.
fn address_is_clean(raw: &str) -> bool {
    !raw.is_empty()
        && raw.chars().count() <= 200
        && raw
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '+'))
        && !raw.contains("..")
}

/// Проверяет адрес до того, как он дойдёт до шлюза. `Err` — текст для честного 404.
pub fn check_address(
    snapshot: &GatewaySnapshot,
    fetched: &FetchedModels,
    raw: &str,
) -> Result<String, String> {
    let raw = raw.trim();
    let not_found =
        || format!("model '{raw}' not found — see GET /v1/models for what this server exposes");
    if !address_is_clean(raw) {
        return Err(not_found());
    }
    let (head, tail) = raw.split_once('/').ok_or_else(not_found)?;
    if tail.is_empty() {
        return Err(not_found());
    }
    if head == "auto" {
        let route = RouteId::parse(tail).map_err(|_| not_found())?;
        let served = snapshot
            .routes
            .iter()
            .find(|r| r.id == route)
            .is_some_and(|r| route_is_served(r, snapshot));
        return served.then(|| raw.to_string()).ok_or_else(not_found);
    }
    let account_id = AccountId::parse(head).map_err(|_| not_found())?;
    let account = snapshot
        .accounts
        .iter()
        .find(|a| a.id == account_id && is_served(a))
        .ok_or_else(not_found)?;
    let known = account_models(account, fetched).iter().any(|m| m == tail)
        // Режимы DeepSeek (`-search` и т. п.) в списке не все, но настоящие.
        || (account.provider == AccountProvider::DeepseekWeb
            && DeepseekMode::parse(tail).is_some());
    known.then(|| raw.to_string()).ok_or_else(not_found)
}

// ── Заголовки ───────────────────────────────────────────────────────────

/// Значение заголовка только из видимого ASCII: чужой текст в заголовок не попадёт.
fn header_value(text: &str) -> Option<HeaderValue> {
    let clean =
        !text.is_empty() && text.len() <= 200 && text.bytes().all(|b| (0x21..=0x7e).contains(&b));
    clean.then(|| HeaderValue::from_str(text).ok()).flatten()
}

/// Заголовки ответа: кто ответил и был ли переход. Секретов и адресов здесь нет.
pub fn route_headers(served: &ServedBy) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = Vec::new();
    let mut push = |name: &'static str, value: &str| {
        if let Some(value) = header_value(value) {
            headers.push((HeaderName::from_static(name), value));
        }
    };
    push(HEADER_ACCOUNT, served.target.account.as_str());
    push(HEADER_MODEL, &served.target.model);
    if let Some(route) = &served.route {
        push(HEADER_ROUTE, route.as_str());
    }
    if !served.failed_before.is_empty() {
        let accounts: Vec<&str> = served
            .failed_before
            .iter()
            .map(|(account, _)| account.as_str())
            .collect();
        let reasons: Vec<&str> = served
            .failed_before
            .iter()
            .map(|(_, class)| class.as_str())
            .collect();
        push(HEADER_FALLBACK, &accounts.join(","));
        push(HEADER_FALLBACK_REASON, &reasons.join(","));
    }
    headers
}

// ── Статус ──────────────────────────────────────────────────────────────

fn unix(time: SystemTime) -> u64 {
    crate::gateway::clock::to_unix(time)
}

/// Всё, что уходит клиенту или в открытый статус, не должно нести адрес апстрима:
/// URL и пары `хост:порт` заменяются.
pub(super) fn scrub(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            let bare = word.trim_matches(|c: char| "()[]<>\"',;".contains(c));
            if bare.contains("://") {
                "<url>"
            } else if looks_like_address(bare) {
                "<addr>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `127.0.0.1`, `10.0.0.5:8080`, `[::1]:80`, `host.internal:443`.
fn looks_like_address(token: &str) -> bool {
    if token.parse::<std::net::IpAddr>().is_ok() || token.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    token.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty()
            && host.contains('.')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            && !port.is_empty()
            && port.chars().all(|c| c.is_ascii_digit())
    })
}

fn status_json(status: &AccountStatus, detailed: bool) -> serde_json::Value {
    let (state, until, reason) = match status {
        AccountStatus::Ready => ("ready", None, None),
        AccountStatus::Cooling { until, .. } => ("cooling", Some(*until), None),
        AccountStatus::Exhausted { resets_at } => ("exhausted", *resets_at, None),
        AccountStatus::NeedsAttention { reason } => ("needs_attention", None, Some(reason)),
        AccountStatus::Disabled => ("disabled", None, None),
        AccountStatus::Unavailable { reason } => ("unavailable", None, Some(reason)),
    };
    let mut value = serde_json::json!({ "state": state });
    if detailed {
        value["until_unix"] = until.map(unix).into();
        value["reason"] = reason.map(|r| scrub(r)).into();
    }
    value
}

fn source_label(source: LimitSource) -> &'static str {
    match source {
        LimitSource::Header => "header",
        LimitSource::CliEvent => "cli_event",
        LimitSource::Ledger => "estimate",
        LimitSource::Declared => "declared",
    }
}

fn kind_label(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::Requests => "requests",
        WindowKind::Tokens => "tokens",
        WindowKind::Spend => "spend",
        WindowKind::Percent => "percent",
    }
}

fn limits_json(limits: &LimitSnapshot, now: SystemTime) -> serde_json::Value {
    serde_json::json!({
        "source": source_label(limits.source),
        "age_secs": limits.age(now).as_secs(),
        "plan": limits.plan,
        "windows": limits.windows.iter().map(|w| serde_json::json!({
            "name": w.name,
            "kind": kind_label(w.kind),
            "used_pct": w.used_pct,
            "limit": w.limit,
            "remaining": w.remaining,
            "resets_at_unix": w.resets_at.map(unix),
        })).collect::<Vec<_>>(),
    })
}

fn why_label(why: &Why) -> &'static str {
    match why {
        Why::FirstReady => "first_ready",
        Why::Sticky => "sticky",
        Why::Pinned => "pinned",
        Why::AfterFailure { .. } => "after_failure",
        Why::OnlyCandidate => "only_candidate",
        Why::RouteExhausted => "route_exhausted",
        Why::ConfigInvalid { .. } => "config_invalid",
        Why::Starting => "starting",
    }
}

fn decision_json(decision: &Decision) -> serde_json::Value {
    serde_json::json!({
        "at_unix": unix(decision.at),
        "route": decision.route.as_ref().map(|r| r.as_str()),
        "target": decision.target.as_ref().map(|t| format!("{}/{}", t.account, t.model)),
        "why": why_label(&decision.why),
        "explain": scrub(&decision.explain()),
        "snapshot_age_secs": decision.snapshot_age.map(|a| a.as_secs()),
    })
}

/// Тело статус-эндпоинта. Без `detailed` (у сервера нет ключа) остаются имена и состояния:
/// ни лимитов, ни решений, ни текстов причин.
pub fn status_body(snapshot: &GatewaySnapshot, detailed: bool) -> serde_json::Value {
    let now = snapshot.generated_at;
    let accounts: Vec<serde_json::Value> = snapshot
        .accounts
        .iter()
        .map(|account| {
            let mut value = serde_json::json!({
                "id": account.id.as_str(),
                "provider": account.provider,
                "enabled": account.enabled,
                "served": is_served(account),
                "status": status_json(&account.status, detailed),
            });
            if detailed {
                value["label"] = account.label.clone().into();
                value["in_flight"] = account.in_flight.into();
                value["max_concurrency"] = account.max_concurrency.into();
                value["limits"] = account
                    .limits
                    .as_ref()
                    .map(|l| limits_json(l, now))
                    .unwrap_or(serde_json::Value::Null);
            }
            value
        })
        .collect();
    let mut body = serde_json::json!({
        "object": "pooprusteek.status",
        "generated_at_unix": unix(now),
        "gateway": { "enabled": true },
        "accounts": accounts,
        "routes": snapshot.routes.iter().map(|route| serde_json::json!({
            "id": route.id.as_str(),
            "served": route_is_served(route, snapshot),
            "steps": route.steps.iter().map(|s| s.account.as_str()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    if detailed {
        body["decisions"] = snapshot.recent.iter().map(decision_json).collect();
        body["problems"] = snapshot
            .problems
            .iter()
            .map(|p| {
                serde_json::json!({
                    "severity": format!("{:?}", p.severity).to_lowercase(),
                    "path": p.path,
                    "message": scrub(&p.message),
                })
            })
            .collect();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_loopback(host: &str) -> bool {
        super::super::is_loopback_host(host)
    }

    #[test]
    fn urls_never_survive_scrubbing() {
        assert_eq!(
            scrub("failed at https://10.0.0.5:8080/v1?key=x for a"),
            "failed at <url> for a"
        );
    }

    #[test]
    fn a_header_value_is_visible_ascii_only() {
        assert!(header_value("work-account").is_some());
        assert!(header_value("with space").is_none());
        assert!(header_value("line\nbreak").is_none());
        assert!(header_value("").is_none());
        assert!(header_value("кириллица").is_none());
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("192.168.1.5"));
    }

    #[test]
    fn dirty_addresses_are_refused_before_the_gateway() {
        for raw in ["", "a/b c", "a/b?x=1", "a/../b", "a/b\nc", "a/b\u{0}"] {
            assert!(!address_is_clean(raw), "{raw:?}");
        }
        assert!(address_is_clean("work/gpt-4o:mini"));
        assert!(address_is_clean("router/qwen/qwen-2.5-coder"));
    }
}
