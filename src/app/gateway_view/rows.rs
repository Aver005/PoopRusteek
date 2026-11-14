//! Строки экрана аккаунтов из снимка шлюза: чистые функции без ввода-вывода.
//! Рендер только рисует то, что здесь посчитано; цвета и знаки выбирает он.

use crate::gateway::handle::{
    AccountStatus, AccountView, Decision, GatewaySnapshot, LimitSnapshot, LimitSource, SkipReason,
    Why,
};
use crate::gateway::types::{AccountProvider, Role, Route};
use std::time::{Duration, SystemTime};

/// Старше этого снимок лимитов считается устаревшим (как у движка).
pub const LIMIT_TTL: Duration = Duration::from_secs(120);
/// Пороги цвета шкалы, пока в снимке нет заявленных `soft_pct`/`hard_pct`.
pub const DEFAULT_SOFT_PCT: f32 = 70.0;
pub const DEFAULT_HARD_PCT: f32 = 90.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Dim,
}

/// Знак состояния; рисует его рендер, чтобы ASCII-режим не ломался.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Ready,
    Cooling,
    Down,
    Attention,
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusText {
    pub mark: Mark,
    pub text: String,
    pub tone: Tone,
}

/// Одно окно лимита. `estimate` — оценка, а не показание сервера; она рисуется с `~`.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitLine {
    pub name: String,
    pub pct: Option<f32>,
    pub estimate: bool,
    pub stale: bool,
    pub resets: Option<String>,
    /// «live · 5 s», «est · 41 s», «declared».
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub id: String,
    pub label: String,
    pub kind: &'static str,
    pub status: StatusText,
    pub limits: Vec<LimitLine>,
    /// Модели: избранные с пометкой, иначе модель по умолчанию.
    pub models: Vec<(String, bool)>,
    pub note: Option<String>,
    pub enabled: bool,
}

/// `API`, `CLI` или `WEB`.
pub fn kind_tag(provider: AccountProvider) -> &'static str {
    match provider {
        AccountProvider::DeepseekWeb => "WEB",
        p if p.is_cli() => "CLI",
        _ => "API",
    }
}

/// `2m 10s`, `1h 12m`, `4d 03h`, `38s`.
pub fn duration_text(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        3600..86_400 => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d {:02}h", secs / 86_400, (secs % 86_400) / 3600),
    }
}

fn until(now: SystemTime, at: SystemTime) -> Option<String> {
    at.duration_since(now).ok().map(duration_text)
}

pub fn status_text(view: &AccountView, now: SystemTime) -> StatusText {
    let busy = view.in_flight > 0;
    match &view.status {
        AccountStatus::Ready if busy => StatusText {
            mark: Mark::Ready,
            text: format!("in use {}/{}", view.in_flight, view.max_concurrency),
            tone: Tone::Ok,
        },
        AccountStatus::Ready => StatusText {
            mark: Mark::Ready,
            text: "ready".into(),
            tone: Tone::Ok,
        },
        AccountStatus::Cooling { until: at, .. } => StatusText {
            mark: Mark::Cooling,
            text: match until(now, *at) {
                Some(left) => format!("cooling {left}"),
                None => "cooling".into(),
            },
            tone: Tone::Warn,
        },
        AccountStatus::Exhausted { resets_at } => StatusText {
            mark: Mark::Down,
            text: match resets_at.and_then(|at| until(now, at)) {
                Some(left) => format!("exhausted · resets {left}"),
                None => "exhausted".into(),
            },
            tone: Tone::Bad,
        },
        AccountStatus::NeedsAttention { reason } => StatusText {
            mark: Mark::Attention,
            text: reason.clone(),
            tone: Tone::Bad,
        },
        AccountStatus::Disabled => StatusText {
            mark: Mark::Off,
            text: "off".into(),
            tone: Tone::Dim,
        },
        AccountStatus::Unavailable { reason } => StatusText {
            mark: Mark::Off,
            text: format!("unavailable: {reason}"),
            tone: Tone::Bad,
        },
    }
}

/// Откуда число и как давно: «live · 5 s», «est · 41 s».
fn provenance(source: LimitSource, age: Duration) -> String {
    let word = match source {
        LimitSource::Header | LimitSource::CliEvent => "live",
        LimitSource::Ledger => "est",
        LimitSource::Declared => "declared",
    };
    if source == LimitSource::Declared {
        return word.to_string();
    }
    format!("{word} · {}", duration_text(age))
}

pub fn limit_lines(snapshot: &LimitSnapshot, now: SystemTime) -> Vec<LimitLine> {
    let age = snapshot.age(now);
    let estimate = matches!(snapshot.source, LimitSource::Ledger | LimitSource::Declared);
    let stale = age > LIMIT_TTL;
    // Худшее окно первым: человек ищет, где ближе всего упор.
    let mut windows: Vec<_> = snapshot.windows.iter().collect();
    windows.sort_by(|a, b| {
        b.used_pct
            .unwrap_or(-1.0)
            .partial_cmp(&a.used_pct.unwrap_or(-1.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    windows
        .into_iter()
        .map(|w| LimitLine {
            name: w.name.clone(),
            pct: w.used_pct,
            estimate,
            stale,
            resets: w.resets_at.and_then(|at| until(now, at)),
            provenance: provenance(snapshot.source, age),
        })
        .collect()
}

pub fn account_row(view: &AccountView, now: SystemTime) -> AccountRow {
    let status = status_text(view, now);
    let mut models: Vec<(String, bool)> =
        view.favorites.iter().map(|m| (m.clone(), true)).collect();
    if models.is_empty()
        && let Some(model) = &view.default_model
    {
        models.push((model.clone(), false));
    }
    let note = match &view.status {
        AccountStatus::Cooling { strikes, .. } if *strikes >= 2 => {
            Some(format!("rate limit hit {strikes} times in a row"))
        }
        AccountStatus::NeedsAttention { .. } => Some("Enter: set it up again".to_string()),
        _ => None,
    };
    AccountRow {
        id: view.id.to_string(),
        label: view.label.clone(),
        kind: kind_tag(view.provider),
        limits: view
            .limits
            .as_ref()
            .map(|l| limit_lines(l, now))
            .unwrap_or_default(),
        status,
        models,
        note,
        enabled: view.enabled,
    }
}

pub fn account_rows(snapshot: &GatewaySnapshot, now: SystemTime) -> Vec<AccountRow> {
    snapshot
        .accounts
        .iter()
        .map(|a| account_row(a, now))
        .collect()
}

/// Сводка для шапки: «3 ready · 1 cooling · 1 needs login».
pub fn summary_text(rows: &[AccountRow]) -> String {
    let count = |mark: Mark| rows.iter().filter(|r| r.status.mark == mark).count();
    let mut parts = Vec::new();
    for (n, word) in [
        (count(Mark::Ready), "ready"),
        (count(Mark::Cooling), "cooling"),
        (count(Mark::Down), "exhausted"),
        (count(Mark::Attention), "needs attention"),
        (count(Mark::Off), "off"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    if parts.is_empty() {
        "no accounts".to_string()
    } else {
        parts.join(" · ")
    }
}

// ── Маршруты ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct StepChip {
    pub number: usize,
    pub account: String,
    pub model: Option<String>,
    pub mark: Mark,
    pub tone: Tone,
    /// Сейчас отвечает этот шаг.
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    pub id: String,
    /// Маршрут главной роли.
    pub main: bool,
    pub steps: Vec<StepChip>,
    /// «falls on rate_limit quota … · switch early at 85%».
    pub detail: String,
}

fn account_name(snapshot: &GatewaySnapshot, id: &str) -> String {
    snapshot
        .accounts
        .iter()
        .find(|a| a.id.as_str() == id)
        .map_or_else(|| id.to_string(), |a| a.label.clone())
}

/// Кто отвечал в последний раз по главной роли.
pub fn current_account(snapshot: &GatewaySnapshot) -> Option<String> {
    snapshot
        .recent
        .iter()
        .rev()
        .find_map(|d| d.target.as_ref().map(|t| t.account.to_string()))
}

fn trigger_words(route: &Route) -> String {
    route
        .triggers()
        .iter()
        .map(|t| format!("{t:?}"))
        .map(|name| {
            let mut out = String::new();
            for (i, ch) in name.chars().enumerate() {
                if ch.is_uppercase() && i > 0 {
                    out.push('_');
                }
                out.push(ch.to_ascii_lowercase());
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn route_rows(snapshot: &GatewaySnapshot, now: SystemTime) -> Vec<RouteRow> {
    let main_route = snapshot.roles.route_for(Role::Main).map(|r| r.to_string());
    let current = current_account(snapshot);
    snapshot
        .routes
        .iter()
        .map(|route| {
            let steps = route
                .steps
                .iter()
                .enumerate()
                .map(|(i, step)| {
                    let view = snapshot.accounts.iter().find(|a| a.id == step.account);
                    let status = view.map(|v| status_text(v, now));
                    StepChip {
                        number: i + 1,
                        account: account_name(snapshot, step.account.as_str()),
                        model: step.model.clone(),
                        mark: status.as_ref().map_or(Mark::Off, |s| s.mark),
                        tone: status.as_ref().map_or(Tone::Bad, |s| s.tone),
                        current: current.as_deref() == Some(step.account.as_str()),
                    }
                })
                .collect();
            let mut detail = format!("falls to the next step on: {}", trigger_words(route));
            if let Some(pct) = route.switch_at_pct {
                detail.push_str(&format!(" · switch early at {pct}%"));
            }
            RouteRow {
                id: route.id.to_string(),
                main: main_route.as_deref() == Some(route.id.as_str()),
                steps,
                detail,
            }
        })
        .collect()
}

pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Main => "main",
        Role::Title => "title",
        Role::Summary => "summary",
        Role::Evaluator => "evaluator",
    }
}

pub const ROLES: [Role; 4] = [Role::Main, Role::Title, Role::Summary, Role::Evaluator];

/// `main → default`, у ролей без записи — «inherits main».
pub fn role_rows(snapshot: &GatewaySnapshot) -> Vec<(Role, String)> {
    ROLES
        .iter()
        .map(|role| {
            let own = snapshot.roles.0.get(role).map(|r| r.to_string());
            (*role, own.unwrap_or_else(|| "inherits main".into()))
        })
        .collect()
}

// ── Решение ─────────────────────────────────────────────────────────────

/// Причина пропуска с сроком возврата, если он известен.
pub fn skip_text(reason: &SkipReason, now: SystemTime) -> String {
    match reason {
        SkipReason::Cooling { until: at } => match until(now, *at) {
            Some(left) => format!("cooling down after a rate limit, back in {left}"),
            None => "cooling down after a rate limit".into(),
        },
        SkipReason::Exhausted { resets_at } => match resets_at.and_then(|at| until(now, at)) {
            Some(left) => format!("quota exhausted, resets in {left}"),
            None => "quota exhausted".into(),
        },
        SkipReason::Disabled => "switched off".into(),
        SkipReason::NeedsAttention => "needs attention (login or block)".into(),
        SkipReason::Unavailable => "unavailable".into(),
        SkipReason::Busy => "busy with another call".into(),
        SkipReason::ContextTooSmall { need, have } => {
            format!("context window {have} is below the {need} needed")
        }
        SkipReason::ToolModeMismatch => "tool protocol does not match the turn".into(),
        SkipReason::NotPureModel => "cannot run as a plain model".into(),
        SkipReason::OverSoftLimit { used_pct, age } => {
            format!("at {used_pct:.0}% (reading {} old)", duration_text(*age))
        }
        SkipReason::OverHardLimit { used_pct, age } => format!(
            "over its hard limit at {used_pct:.0}% (reading {} old)",
            duration_text(*age)
        ),
        SkipReason::NotForServe => "not served to outside clients".into(),
        SkipReason::InvalidConfig { message } => format!("config error: {message}"),
        SkipReason::NoAttachmentSupport => "cannot take the attached file".into(),
        SkipReason::FailedThisCall { class } => format!("just failed ({class})"),
        SkipReason::PinnedElsewhere => "another account is pinned".into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionView {
    /// «14:02:31 · limits read 5 s ago».
    pub head: String,
    /// Главная строка: кого взяли и почему, или что выбирать некого.
    pub headline: String,
    pub ok: bool,
    pub skipped: Vec<String>,
    /// `Decision::explain()` как есть.
    pub raw: String,
}

fn clock_text(at: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format("%H:%M:%S").to_string()
}

pub fn decision_view(
    snapshot: &GatewaySnapshot,
    decision: &Decision,
    now: SystemTime,
) -> DecisionView {
    let mut head = clock_text(decision.at);
    if let Some(age) = decision.snapshot_age {
        head.push_str(&format!(" · limits read {} ago", duration_text(age)));
    }
    let (headline, ok) = match (&decision.target, &decision.why) {
        (Some(t), why) => {
            let reason = match why {
                Why::FirstReady => "first ready step".to_string(),
                Why::Sticky => "the chat stays on it".to_string(),
                Why::Pinned => "pinned".to_string(),
                Why::AfterFailure { from, class } => {
                    format!("{} failed ({class})", account_name(snapshot, from.as_str()))
                }
                _ => "the only candidate left".to_string(),
            };
            (
                format!(
                    "{} · {} — {reason}",
                    account_name(snapshot, t.account.as_str()),
                    t.model
                ),
                true,
            )
        }
        (None, Why::ConfigInvalid { message }) => (format!("route is off: {message}"), false),
        (None, Why::Starting) => ("the gateway is starting".to_string(), false),
        (None, _) => (
            match decision.retry_at.and_then(|at| until(now, at)) {
                Some(left) => format!("no account available · next one back in {left}"),
                None => "no account available".to_string(),
            },
            false,
        ),
    };
    DecisionView {
        head,
        headline,
        ok,
        skipped: decision
            .skipped
            .iter()
            .map(|s| {
                format!(
                    "{}: {}",
                    account_name(snapshot, s.account.as_str()),
                    skip_text(&s.reason, now)
                )
            })
            .collect(),
        raw: decision.explain(),
    }
}

/// Последнее решение для показа, если оно было.
pub fn last_decision(snapshot: &GatewaySnapshot, now: SystemTime) -> Option<DecisionView> {
    snapshot
        .recent
        .last()
        .map(|d| decision_view(snapshot, d, now))
}

/// Модель последнего выбора шлюза, а если выборов ещё не было, модель первого шага главного маршрута.
pub fn last_decision_model(snapshot: &GatewaySnapshot) -> Option<String> {
    if let Some(target) = snapshot.recent.iter().rev().find_map(|d| d.target.as_ref()) {
        return Some(target.model.clone());
    }
    let step = super::edits::main_route(snapshot)?.steps.first()?;
    step.model.clone().or_else(|| {
        snapshot
            .accounts
            .iter()
            .find(|a| a.id == step.account)
            .and_then(|a| a.default_model.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::types::{AccountId, Roles};

    fn id(raw: &str) -> AccountId {
        AccountId::parse(raw).unwrap()
    }

    fn view(label: &str, status: AccountStatus) -> AccountView {
        AccountView {
            id: id(label),
            label: label.into(),
            provider: AccountProvider::CliClaude,
            enabled: true,
            status,
            limits: None,
            in_flight: 0,
            max_concurrency: 1,
            default_model: None,
            favorites: vec!["opus-5-5".into()],
            capabilities: None,
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn durations_read_like_a_person_would() {
        assert_eq!(duration_text(Duration::from_secs(38)), "38s");
        assert_eq!(duration_text(Duration::from_secs(130)), "2m 10s");
        assert_eq!(duration_text(Duration::from_secs(4_320)), "1h 12m");
        assert_eq!(
            duration_text(Duration::from_secs(4 * 86_400 + 3 * 3600)),
            "4d 03h"
        );
    }

    #[test]
    fn status_is_a_word_and_a_mark_not_only_a_colour() {
        let now = at(1000);
        let cooling = view(
            "work",
            AccountStatus::Cooling {
                until: at(1130),
                strikes: 2,
            },
        );
        let status = status_text(&cooling, now);
        assert_eq!(status.text, "cooling 2m 10s");
        assert_eq!(status.mark, Mark::Cooling);
        let row = account_row(&cooling, now);
        assert_eq!(row.note.as_deref(), Some("rate limit hit 2 times in a row"));
        let mut busy = view("max", AccountStatus::Ready);
        busy.in_flight = 1;
        assert_eq!(status_text(&busy, now).text, "in use 1/1");
    }

    #[test]
    fn an_estimate_is_marked_and_a_stale_reading_is_flagged() {
        let now = at(10_000);
        let snap = LimitSnapshot {
            account: id("a"),
            plan: None,
            windows: vec![
                crate::gateway::handle::LimitWindow {
                    name: "week".into(),
                    kind: crate::gateway::handle::WindowKind::Percent,
                    used_pct: Some(18.0),
                    limit: None,
                    remaining: None,
                    resets_at: Some(at(10_000 + 4 * 86_400)),
                },
                crate::gateway::handle::LimitWindow {
                    name: "5h".into(),
                    kind: crate::gateway::handle::WindowKind::Percent,
                    used_pct: Some(52.0),
                    limit: None,
                    remaining: None,
                    resets_at: None,
                },
            ],
            observed_at: at(9_995),
            source: LimitSource::CliEvent,
        };
        let lines = limit_lines(&snap, now);
        assert_eq!(lines[0].name, "5h", "the busiest window goes first");
        assert!(!lines[0].estimate && !lines[0].stale);
        assert_eq!(lines[0].provenance, "live · 5s");
        let estimate = LimitSnapshot {
            source: LimitSource::Ledger,
            observed_at: at(9_000),
            ..snap
        };
        let lines = limit_lines(&estimate, now);
        assert!(lines[0].estimate && lines[0].stale);
        assert!(lines[0].provenance.starts_with("est"));
    }

    fn snapshot(accounts: Vec<AccountView>, recent: Vec<Decision>) -> GatewaySnapshot {
        GatewaySnapshot {
            settings: Default::default(),
            accounts,
            routes: Vec::new(),
            roles: Roles::default(),
            recent,
            problems: Vec::new(),
            generated_at: at(0),
        }
    }

    #[test]
    fn a_skipped_account_says_when_it_comes_back() {
        let snap = snapshot(vec![view("oc", AccountStatus::Ready)], Vec::new());
        let decision = Decision {
            route: None,
            target: Some(crate::gateway::handle::Target {
                account: id("oc"),
                model: "m".into(),
            }),
            tool_mode: crate::gateway::types::ToolMode::Prompt,
            why: Why::FirstReady,
            skipped: vec![crate::gateway::handle::Skipped {
                account: id("oc"),
                reason: SkipReason::Cooling { until: at(1130) },
            }],
            snapshot_age: Some(Duration::from_secs(5)),
            retry_at: None,
            at: at(1000),
        };
        let view = decision_view(&snap, &decision, at(1000));
        assert!(
            view.skipped[0].contains("back in 2m 10s"),
            "{:?}",
            view.skipped
        );
        assert!(view.head.contains("limits read 5s ago"), "{}", view.head);
        assert!(view.ok);
        assert!(
            view.raw.contains("first ready step"),
            "explain() is shown as is"
        );
    }

    #[test]
    fn nobody_available_shows_when_the_next_one_returns() {
        let snap = snapshot(Vec::new(), Vec::new());
        let decision = Decision {
            route: None,
            target: None,
            tool_mode: crate::gateway::types::ToolMode::Prompt,
            why: Why::RouteExhausted,
            skipped: Vec::new(),
            snapshot_age: None,
            retry_at: Some(at(1130)),
            at: at(1000),
        };
        let view = decision_view(&snap, &decision, at(1000));
        assert!(!view.ok);
        assert_eq!(
            view.headline,
            "no account available · next one back in 2m 10s"
        );
    }

    #[test]
    fn the_kind_tag_separates_api_cli_and_web() {
        assert_eq!(kind_tag(AccountProvider::Openai), "API");
        assert_eq!(kind_tag(AccountProvider::CliOpencode), "CLI");
        assert_eq!(kind_tag(AccountProvider::DeepseekWeb), "WEB");
    }

    #[test]
    fn the_summary_counts_by_state() {
        let now = at(0);
        let rows = vec![
            account_row(&view("a", AccountStatus::Ready), now),
            account_row(&view("b", AccountStatus::Ready), now),
            account_row(&view("c", AccountStatus::Disabled), now),
        ];
        assert_eq!(summary_text(&rows), "2 ready · 1 off");
        assert_eq!(summary_text(&[]), "no accounts");
    }
}
