//! Сцены снимков для экрана аккаунтов: данные выдуманы, часы заморожены.

use crate::app::AppState;
use crate::app::events::{Modal, View};
use crate::app::gateway_view::forms::{LimitsForm, SecretPrompt, TrustDialog};
use crate::app::gateway_view::wizard::{AccountWizard, Step};
use crate::app::gateway_view::{Focus, SEAM_STATUS};
use crate::config::{Config, TrustRequest};
use crate::gateway::handle::{
    AccountStatus, AccountView, Decision, GatewaySnapshot, LimitSnapshot, LimitSource, LimitWindow,
    SkipReason, Skipped, Target, Why, WindowKind,
};
use crate::gateway::types::{
    Account, AccountId, AccountProvider, Role, Roles, Route, RouteId, Step as RouteStep, ToolMode,
};
use crate::provider::ChatMessage;
use std::time::{Duration, SystemTime};

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).expect("demo id")
}

fn window(name: &str, pct: f32, resets_in: Option<u64>, kind: WindowKind) -> LimitWindow {
    LimitWindow {
        name: name.into(),
        kind,
        used_pct: Some(pct),
        limit: None,
        remaining: None,
        resets_at: resets_in.map(|s| now() + Duration::from_secs(s)),
    }
}

fn limits(
    account: &str,
    source: LimitSource,
    age: u64,
    windows: Vec<LimitWindow>,
) -> LimitSnapshot {
    LimitSnapshot {
        account: id(account),
        plan: None,
        windows,
        observed_at: now() - Duration::from_secs(age),
        source,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "демо-данные: одна строка на аккаунт"
)]
fn view(
    name: &str,
    provider: AccountProvider,
    status: AccountStatus,
    in_flight: u32,
    limits: Option<LimitSnapshot>,
    favorites: &[&str],
    default_model: Option<&str>,
) -> AccountView {
    AccountView {
        id: id(name),
        label: name.into(),
        provider,
        enabled: !matches!(status, AccountStatus::Disabled),
        status,
        limits,
        in_flight,
        max_concurrency: 1,
        default_model: default_model.map(str::to_string),
        favorites: favorites.iter().map(|m| m.to_string()).collect(),
        capabilities: None,
    }
}

fn step(account: &str, model: Option<&str>) -> RouteStep {
    RouteStep {
        account: id(account),
        model: model.map(str::to_string),
    }
}

fn route(name: &str, steps: Vec<RouteStep>, switch_at: Option<u8>) -> Route {
    Route {
        id: RouteId::parse(name).expect("demo route"),
        strategy: Default::default(),
        steps,
        on: Vec::new(),
        switch_at_pct: switch_at,
        allow_mixed_tools: false,
    }
}

/// Пять аккаунтов: две подписки Claude, OpenCode на паузе, API с оценкой расхода и DeepSeek.
pub(super) fn demo_snapshot() -> GatewaySnapshot {
    let accounts = vec![
        view(
            "claude-max",
            AccountProvider::CliClaude,
            AccountStatus::Ready,
            1,
            Some(limits(
                "claude-max",
                LimitSource::CliEvent,
                5,
                vec![
                    window("5h", 52.0, Some(4_320), WindowKind::Percent),
                    window(
                        "week",
                        18.0,
                        Some(4 * 86_400 + 3 * 3_600),
                        WindowKind::Percent,
                    ),
                ],
            )),
            &["opus-5-5", "sonnet-5-5"],
            None,
        ),
        view(
            "opencode-go",
            AccountProvider::CliOpencode,
            AccountStatus::Cooling {
                until: now() + Duration::from_secs(130),
                strikes: 2,
            },
            0,
            Some(limits(
                "opencode-go",
                LimitSource::CliEvent,
                41,
                vec![
                    window("5h", 91.0, Some(2_280), WindowKind::Percent),
                    window(
                        "week",
                        40.0,
                        Some(2 * 86_400 + 11 * 3_600),
                        WindowKind::Percent,
                    ),
                ],
            )),
            &["deepseek-v4.1-flash"],
            None,
        ),
        view(
            "anthropic-api",
            AccountProvider::Anthropic,
            AccountStatus::Ready,
            0,
            Some(limits(
                "anthropic-api",
                LimitSource::Ledger,
                12,
                vec![window("spend", 37.0, None, WindowKind::Spend)],
            )),
            &["claude-sonnet-5-5"],
            Some("claude-sonnet-5-5"),
        ),
        view(
            "deepseek",
            AccountProvider::DeepseekWeb,
            AccountStatus::Ready,
            0,
            None,
            &["deepseek-chat", "deepseek-reasoner"],
            Some("deepseek-chat"),
        ),
        view(
            "claude-work",
            AccountProvider::CliClaude,
            AccountStatus::NeedsAttention {
                reason: "login expired".into(),
            },
            0,
            Some(limits(
                "claude-work",
                LimitSource::CliEvent,
                2 * 86_400,
                vec![window("5h", 12.0, None, WindowKind::Percent)],
            )),
            &[],
            None,
        ),
    ];
    let decision = Decision {
        route: Some(RouteId::parse("default").expect("route")),
        target: Some(Target {
            account: id("claude-max"),
            model: "opus-5-5".into(),
        }),
        tool_mode: ToolMode::Prompt,
        why: Why::FirstReady,
        skipped: vec![Skipped {
            account: id("opencode-go"),
            reason: SkipReason::Cooling {
                until: now() + Duration::from_secs(130),
            },
        }],
        snapshot_age: Some(Duration::from_secs(5)),
        retry_at: None,
        at: now() - Duration::from_secs(3),
    };
    let mut roles = Roles::default();
    for (role, name) in [
        (Role::Main, "default"),
        (Role::Title, "cheap"),
        (Role::Summary, "cheap"),
        (Role::Evaluator, "default"),
    ] {
        roles.0.insert(role, RouteId::parse(name).expect("route"));
    }
    GatewaySnapshot {
        settings: Default::default(),
        accounts,
        routes: vec![
            route(
                "default",
                vec![
                    step("claude-max", Some("opus-5-5")),
                    step("opencode-go", Some("flash")),
                    step("anthropic-api", Some("sonnet-5-5")),
                    step("deepseek", Some("chat")),
                ],
                Some(85),
            ),
            route(
                "cheap",
                vec![
                    step("deepseek", Some("chat")),
                    step("opencode-go", Some("flash")),
                ],
                None,
            ),
        ],
        roles,
        recent: vec![decision],
        problems: Vec::new(),
        generated_at: now(),
    }
}

fn demo_config(config: &mut Config) {
    let mut max = Account::new(id("claude-max"), AccountProvider::CliClaude);
    max.home = Some(std::path::PathBuf::from(
        r"C:\Users\demo\.pooprusteek\accounts\claude-max",
    ));
    max.limits.soft_pct = Some(70);
    max.limits.hard_pct = Some(90);
    let mut api = Account::new(id("anthropic-api"), AccountProvider::Anthropic);
    api.base_url = Some("https://api.anthropic.com/v1".into());
    config.accounts = vec![max, api];
}

pub(super) fn with_snapshot(state: &mut AppState, config: &mut Config) {
    demo_config(config);
    state.gateway_view.snapshot = Some(demo_snapshot());
    state.gateway_view.enabled = true;
}

pub(super) fn accounts(base: AppState, config: &mut Config) -> AppState {
    let mut state = base;
    with_snapshot(&mut state, config);
    state.view = View::Accounts;
    state.gateway_view.screen.selected = 0;
    state
}

pub(super) fn accounts_second_selected(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    state.gateway_view.screen.selected = 1;
    state.gateway_view.screen.say("Rebuilt opencode-go.");
    state
}

pub(super) fn accounts_details(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    state.gateway_view.screen.detail = true;
    state
}

pub(super) fn accounts_routes(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    state.gateway_view.screen.focus = Focus::Routes;
    state.gateway_view.screen.step = 1;
    state
}

pub(super) fn accounts_off(base: AppState, _config: &mut Config) -> AppState {
    let mut state = base;
    state.view = View::Accounts;
    state.gateway_view.enabled = false;
    state
}

pub(super) fn landing_accounts(base: AppState, config: &mut Config) -> AppState {
    let mut state = base;
    with_snapshot(&mut state, config);
    state
}

/// Две подписки Claude в мастере: свой вход для второй, окно подтверждения.
pub(super) fn wizard_kind(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    state.modal = Some(Modal::AccountWizard(Box::default()));
    state
}

pub(super) fn wizard_confirm(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    let mut wizard = AccountWizard::new();
    wizard.kind = 1;
    wizard.label.insert_str("claude-second");
    wizard.step = Step::Confirm;
    state.modal = Some(Modal::AccountWizard(Box::new(wizard)));
    state
}

pub(super) fn wizard_secret(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    let mut wizard = AccountWizard::new();
    wizard.label.insert_str("local lm");
    wizard.base_url.insert_str("http://localhost:1234/v1");
    wizard.step = Step::Secret;
    wizard.secret.push_str("sk-demo-secret-value");
    state.modal = Some(Modal::AccountWizard(Box::new(wizard)));
    state
}

pub(super) fn wizard_direct(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    let mut wizard = AccountWizard::new();
    wizard.kind = 3;
    wizard.label.insert_str("codex-token");
    wizard.step = Step::Consent;
    state.modal = Some(Modal::AccountWizard(Box::new(wizard)));
    state
}

pub(super) fn limits_form(base: AppState, config: &mut Config) -> AppState {
    let mut state = accounts(base, config);
    let account = config.accounts[0].clone();
    state.modal = Some(Modal::AccountLimits(Box::new(LimitsForm::for_account(
        &account,
    ))));
    state
}

pub(super) fn secret_prompt(base: AppState, _config: &mut Config) -> AppState {
    let mut state = base;
    let mut prompt = SecretPrompt::named("anthropic-api");
    prompt.value.push_str("sk-ant-demo-123456");
    state.modal = Some(Modal::SecretPrompt(Box::new(prompt)));
    state
}

pub(super) fn trust(base: AppState, _config: &mut Config) -> AppState {
    let mut state = base;
    state.modal = Some(Modal::Trust(Box::new(TrustDialog::new(TrustRequest {
        config_path: std::path::PathBuf::from(r"D:\Projects\demo-api\.pooprusteek\config.toml"),
        key: "demo".into(),
        hash: "abc".into(),
        held_back: vec![
            "account \"team-proxy\" sends requests to https://llm.team.example/v1".into(),
            "route \"default\" now starts with team-proxy".into(),
            "role main -> default".into(),
        ],
    }))));
    state
}

pub(super) fn usage(base: AppState, config: &mut Config) -> AppState {
    let mut state = base;
    with_snapshot(&mut state, config);
    let rows = vec![
        crate::gateway::handle::LedgerRow {
            account: id("claude-max"),
            model: "opus-5-5".into(),
            source: crate::gateway::handle::CallSource::Chat,
            calls: 41,
            failed: 0,
            fallbacks: 0,
            prompt_tokens: 1_200_000,
            completion_tokens: 96_000,
            cache_read_tokens: 936_000,
            any_estimated: false,
        },
        crate::gateway::handle::LedgerRow {
            account: id("opencode-go"),
            model: "flash".into(),
            source: crate::gateway::handle::CallSource::Chat,
            calls: 12,
            failed: 3,
            fallbacks: 3,
            prompt_tokens: 210_000,
            completion_tokens: 18_000,
            cache_read_tokens: 0,
            any_estimated: true,
        },
    ];
    let labels = demo_snapshot()
        .accounts
        .iter()
        .map(|a| (a.id.to_string(), a.label.clone()))
        .collect();
    let text = crate::app::gateway_view::usage_table(&rows, &labels);
    state.focused_mut().messages = vec![ChatMessage::user("/usage"), ChatMessage::ui_system(&text)];
    state
}

/// Шов в истории: ответы двух аккаунтов и строка о переключении между ними.
pub(super) fn seam(base: AppState, config: &mut Config) -> AppState {
    let mut state = base;
    with_snapshot(&mut state, config);
    let mut seam = ChatMessage::ui_system(&crate::app::gateway_view::seam_text(
        "claude-max",
        "opencode-go",
        "rate_limit",
        Some("2m 10s"),
    ));
    seam.status = Some(SEAM_STATUS.to_string());
    state.focused_mut().messages = vec![
        ChatMessage::user("Почини падающий тест parse_range"),
        ChatMessage::assistant("Смотрю тест и код: `parse_range` падает на пустой строке."),
        seam,
        ChatMessage::assistant("Продолжаю на запасном аккаунте: правка уже в файле."),
    ];
    state
}
