//! Сквозные проверки экрана аккаунтов: настоящий `App`, шлюз на заглушках, файл конфига во временной папке.

use super::*;
use crate::config::ProviderEntry;
use crate::gateway::handle::AccountStatus;
use crate::gateway::pending::{PendingGateway, PendingHandle};
use crate::gateway::testkit::{Act, FakeBackend, TWO_ACCOUNTS, ask, parse, rig};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

async fn app_with_file(tag: &str) -> (App, std::path::PathBuf) {
    let mut config = Config::default();
    config.semantic.enabled = false;
    config.update.auto = false;
    let dir = std::env::temp_dir().join(format!(
        "pooprusteek-flow-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    config.source.write_path = Some(path.clone());
    (App::new(config).await.expect("the app starts"), path)
}

async fn wizard_key(app: &mut App, code: KeyCode) {
    let Some(Modal::AccountWizard(wizard)) = app.state.modal.take() else {
        panic!("the wizard is closed");
    };
    app.handle_account_wizard_key(press(code), wizard)
        .await
        .unwrap();
}

async fn wizard_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        wizard_key(app, KeyCode::Char(ch)).await;
    }
}

/// API-аккаунт: тип, имя, протокол, адрес, модель, ключ (может быть пустым), подтверждение.
async fn run_api_wizard(app: &mut App, name: &str, key: &str) {
    app.state.modal = Some(Modal::AccountWizard(Box::default()));
    wizard_key(app, KeyCode::Enter).await;
    wizard_text(app, name).await;
    wizard_key(app, KeyCode::Enter).await;
    wizard_key(app, KeyCode::Enter).await;
    wizard_text(app, "http://localhost:1234/v1").await;
    wizard_key(app, KeyCode::Enter).await;
    wizard_text(app, "qwen").await;
    wizard_key(app, KeyCode::Enter).await;
    wizard_text(app, key).await;
    wizard_key(app, KeyCode::Enter).await;
    wizard_key(app, KeyCode::Enter).await;
}

async fn next_event(app: &mut App) -> AppEvent {
    tokio::time::timeout(Duration::from_secs(60), app.event_rx.recv())
        .await
        .expect("an event arrives")
        .expect("channel open")
}

fn provider_names(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    crate::config::parse(&text)
        .map(|c| c.providers.into_iter().map(|p| p.name).collect())
        .unwrap_or_default()
}

fn lm_entry() -> ProviderEntry {
    ProviderEntry {
        name: "lm".into(),
        base_url: "http://localhost:1234/v1".into(),
        api_key: None,
        model: "m1".into(),
        protocol: Default::default(),
        tools: Default::default(),
    }
}

// ── Шлюз выключен: `/accounts` и `/providers add` работают ──────────────

#[tokio::test]
async fn with_the_gateway_off_the_wizard_writes_a_providers_entry_and_nothing_else() {
    let (mut app, path) = app_with_file("legacy-add").await;
    assert!(!app.gateway.is_enabled());
    run_api_wizard(&mut app, "Local LM", "").await;
    let AppEvent::LegacyProviderStored(plan) = next_event(&mut app).await else {
        panic!("a [[providers]] entry is expected, not an account");
    };
    app.on_legacy_provider_stored(*plan);

    assert_eq!(provider_names(&path), ["local-lm"], "written as before");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("[[accounts]]") && !text.contains("[gateway]"),
        "{text}"
    );
    assert_eq!(app.config.providers[0].name, "local-lm");
    assert_eq!(app.config.providers[0].model, "qwen");
    assert!(!app.gateway.is_enabled(), "the gateway stays off");
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(notice.text.contains("Added local-lm"), "{}", notice.text);
}

/// Нет хранилища: ключ не лёг, а запись, которую ради него добавили, ушла — сирот нет.
#[tokio::test]
async fn a_key_that_cannot_be_stored_leaves_no_entry_behind() {
    let (mut app, path) = app_with_file("legacy-orphan").await;
    run_api_wizard(&mut app, "Keyed", "sk-test-canary-0123456789").await;
    let AppEvent::LegacyProviderStored(plan) = next_event(&mut app).await else {
        panic!("expected the legacy plan");
    };
    assert!(plan.stored.is_err(), "the test store refuses writes");
    app.on_legacy_provider_stored(*plan);
    assert!(provider_names(&path).is_empty(), "the entry was taken back");
    assert!(app.config.providers.is_empty());
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(
        notice.error && notice.text.contains("not stored"),
        "{}",
        notice.text
    );
    assert!(
        !notice.text.contains("canary"),
        "the key never reaches the screen"
    );
}

#[tokio::test]
async fn a_subscription_needs_the_gateway_and_the_wizard_says_so() {
    let (mut app, _) = app_with_file("legacy-sub").await;
    app.state.modal = Some(Modal::AccountWizard(Box::default()));
    wizard_key(&mut app, KeyCode::Down).await;
    wizard_key(&mut app, KeyCode::Enter).await;
    let Some(Modal::AccountWizard(wizard)) = &app.state.modal else {
        panic!("the wizard stays open");
    };
    assert!(
        wizard
            .error
            .as_deref()
            .unwrap_or("")
            .contains("need the gateway"),
        "{:?}",
        wizard.error
    );
}

#[tokio::test]
async fn the_off_screen_switches_the_active_provider_and_turns_the_gateway_on_only_after_y() {
    let (mut app, path) = app_with_file("legacy-screen").await;
    app.config.providers.push(lm_entry());
    app.open_accounts(Focus::Accounts);

    // Вторая строка — «lm»: Enter делает её активной.
    app.handle_accounts_key(press(KeyCode::Down)).await.unwrap();
    app.handle_accounts_key(press(KeyCode::Enter))
        .await
        .unwrap();
    assert_eq!(app.config.active_provider.as_deref(), Some("lm"));
    // Назад на встроенный.
    app.handle_accounts_key(press(KeyCode::Up)).await.unwrap();
    app.handle_accounts_key(press(KeyCode::Enter))
        .await
        .unwrap();
    assert_eq!(app.config.active_provider, None);

    // `g` спрашивает; всё, кроме `y`, оставляет шлюз выключенным.
    app.handle_accounts_key(press(KeyCode::Char('g')))
        .await
        .unwrap();
    assert!(app.state.gateway_view.screen.confirm_enable);
    app.handle_accounts_key(press(KeyCode::Char('n')))
        .await
        .unwrap();
    assert!(!app.config.gateway.enabled && !app.gateway.is_enabled());

    app.handle_accounts_key(press(KeyCode::Char('g')))
        .await
        .unwrap();
    app.handle_accounts_key(press(KeyCode::Char('y')))
        .await
        .unwrap();
    let next = enabled_event(&mut app)
        .await
        .expect("the enabling went through");
    app.on_gateway_enabled(Ok(next));
    assert!(app.config.gateway.enabled, "confirmed");
    assert!(
        app.config.accounts.iter().any(|a| a.id.as_str() == "lm"),
        "the provider added with the gateway off becomes an account"
    );
    assert!(
        app.config.providers.iter().any(|p| p.name == "lm"),
        "and is kept"
    );
    let _ = path;
}

#[tokio::test]
async fn models_of_a_providers_entry_are_saved_into_the_file() {
    let (mut app, path) = app_with_file("legacy-model").await;
    crate::config::add_provider_at(&path, &lm_entry(), None).unwrap();
    app.config.providers.push(lm_entry());
    app.config.active_provider = Some("lm".into());
    app.apply_model_switch("m2");
    let text = std::fs::read_to_string(&path).unwrap();
    let typed = crate::config::parse(&text).unwrap();
    assert_eq!(
        typed.providers[0].model, "m2",
        "it survives a restart: {text}"
    );
    assert_eq!(app.config.providers[0].model, "m2");

    // Обратная сторона: записи в файле нет — память не меняется и ошибка названа.
    app.config.providers[0].name = "ghost".into();
    app.config.active_provider = Some("ghost".into());
    app.apply_model_switch("m3");
    assert_eq!(app.config.providers[0].model, "m2");
}

#[tokio::test]
async fn tools_of_a_providers_entry_change_in_the_file_while_the_gateway_is_off() {
    let (mut app, path) = app_with_file("legacy-tools").await;
    crate::config::add_provider_at(&path, &lm_entry(), None).unwrap();
    app.config.providers.push(lm_entry());
    app.set_account_tools("lm", crate::config::ToolProtocol::Native);
    let typed = crate::config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        typed.providers[0].tools,
        crate::config::ToolProtocol::Native
    );
    assert_eq!(
        app.config.providers[0].tools,
        crate::config::ToolProtocol::Native
    );
}

// ── Шлюз собирается ─────────────────────────────────────────────────────

#[tokio::test]
async fn while_the_gateway_starts_the_wizard_says_wait_and_nothing_is_written() {
    let (mut app, _) = app_with_file("starting").await;
    let config = parse(TWO_ACCOUNTS);
    app.gateway = Arc::new(PendingHandle(PendingGateway::new(config)));
    app.config.gateway.enabled = true;
    app.refresh_gateway_view();

    run_api_wizard(&mut app, "Late", "sk-test-canary-0123456789").await;
    let Some(Modal::AccountWizard(wizard)) = &app.state.modal else {
        panic!("the wizard stays open and keeps what was typed");
    };
    assert!(
        wizard
            .error
            .as_deref()
            .unwrap_or("")
            .contains("still starting"),
        "{:?}",
        wizard.error
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), app.event_rx.recv())
            .await
            .is_err(),
        "no secret was written and no plan was sent"
    );
    // Кнопка `a` на экране тоже не открывает мастер вслепую.
    app.state.modal = None;
    app.open_accounts(Focus::Accounts);
    app.handle_accounts_key(press(KeyCode::Char('a')))
        .await
        .unwrap();
    assert!(app.state.modal.is_none());
    assert!(app.state.gateway_view.screen.notice.as_ref().unwrap().error);
}

/// Кандидат проверяется до записи секрета: отказ конфига не оставляет ключа в хранилище.
#[tokio::test]
async fn an_account_the_config_would_refuse_is_stopped_before_the_secret_is_written() {
    let (mut app, _) = app_with_file("candidate").await;
    let rig = rig("[gateway]\nenabled = true\n", Vec::new());
    app.gateway = Arc::new(rig.engine);
    app.config.gateway.enabled = true;
    app.refresh_gateway_view();
    app.state.modal = Some(Modal::AccountWizard(Box::default()));
    // Прямой токен на адрес Claude: конфиг такое отвергает.
    let mut wizard = wizard::AccountWizard::new();
    wizard.kind = 3;
    wizard.label.insert_str("tok");
    wizard.base_url.insert_str("https://api.anthropic.com/v1");
    wizard.model.insert_str("m");
    wizard.consent_choice = 1;
    wizard.secret.push_str("tok-abcdefghijkl");
    app.submit_account_wizard(Box::new(wizard));
    let Some(Modal::AccountWizard(wizard)) = &app.state.modal else {
        panic!("the wizard reopens with the reason");
    };
    assert!(
        wizard.error.as_deref().unwrap_or("").contains("Claude"),
        "{:?}",
        wizard.error
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), app.event_rx.recv())
            .await
            .is_err(),
        "nothing was stored"
    );
}

// ── Удаление аккаунта, шаги маршрутов, закрепления ──────────────────────

async fn two_accounts_app() -> App {
    let (mut app, _) = app_with_file("two").await;
    let rig = rig(
        TWO_ACCOUNTS,
        vec![
            (
                "a",
                FakeBackend::new(vec![Act::Reply("from a"), Act::Reply("from a")]),
            ),
            (
                "b",
                FakeBackend::new(vec![Act::Reply("from b"), Act::Reply("from b")]),
            ),
        ],
    );
    let config = parse(TWO_ACCOUNTS);
    app.config.gateway.enabled = true;
    app.config.accounts = config.accounts.clone();
    app.config.routes = config.routes.clone();
    app.config.roles = config.roles.clone();
    app.gateway = Arc::new(rig.engine);
    app.refresh_gateway_view();
    app
}

#[tokio::test]
async fn the_remove_question_names_the_routes_and_the_removal_takes_the_steps_along() {
    let mut app = two_accounts_app().await;
    let a = AccountId::parse("a").unwrap();
    app.ask_remove_account(&a);
    let text = app
        .state
        .gateway_view
        .screen
        .confirm_remove
        .clone()
        .unwrap()
        .text;
    assert!(text.contains("route main (role main)"), "{text}");
    assert!(text.contains("steps"), "{text}");

    app.remove_account(&a);
    let snapshot = app.gateway.snapshot();
    assert_eq!(snapshot.accounts.len(), 1);
    assert_eq!(
        snapshot.routes[0].steps.len(),
        1,
        "the step went with the account"
    );
    assert!(
        snapshot
            .problems
            .iter()
            .all(|p| p.severity != Severity::Error),
        "{:?}",
        snapshot.problems
    );
    assert!(!app.state.gateway_view.screen.notice.as_ref().unwrap().error);

    // Единственный шаг: вопроса нет, причина названа.
    app.state.gateway_view.screen.confirm_remove = None;
    app.refresh_gateway_view();
    app.ask_remove_account(&AccountId::parse("b").unwrap());
    assert!(app.state.gateway_view.screen.confirm_remove.is_none());
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(
        notice.error && notice.text.contains("only step of route main (role main)"),
        "{}",
        notice.text
    );
}

#[tokio::test]
async fn removing_a_pinned_account_unpins_the_chat_and_says_so() {
    let mut app = two_accounts_app().await;
    app.pin_outcome("a").unwrap();
    let focused = app.state.conversations.focused_id();
    assert!(app.state.gateway_view.pins.contains_key(&focused));

    app.remove_account(&AccountId::parse("a").unwrap());

    assert!(app.state.gateway_view.pins.is_empty(), "the pin is gone");
    let last = app.state.focused().messages.last().unwrap().content.clone();
    assert!(
        last.contains("Unpinned") && last.contains("removed"),
        "{last}"
    );
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(notice.text.contains("pinned chat"), "{}", notice.text);
    // Чат снова идёт по маршруту, а не умирает на удалённом аккаунте.
    let provider = app.state.focused().provider.clone().expect("a provider");
    let (result, text) = ask(&provider, "hi").await;
    result.unwrap();
    assert_eq!(text, "from b");
}

#[tokio::test]
async fn a_pin_survives_a_config_reload_and_is_rebuilt_on_the_new_gateway() {
    let mut app = two_accounts_app().await;
    app.pin_outcome("b").unwrap();
    app.rebuild_provider();
    let focused = app.state.conversations.focused_id();
    assert!(app.state.gateway_view.pins.contains_key(&focused), "kept");
    let pinned = app.state.focused().provider.clone().unwrap();
    let (_, text) = ask(&pinned, "hi").await;
    assert_eq!(text, "from b", "still on its account, not the route's");

    // Шлюз собрали заново: провайдер чата пересобирается под тот же аккаунт.
    let second = rig(
        TWO_ACCOUNTS,
        vec![
            ("a", FakeBackend::new(vec![Act::Reply("new a")])),
            ("b", FakeBackend::new(vec![Act::Reply("new b")])),
        ],
    );
    app.gateway = Arc::new(second.engine);
    app.rebuild_provider();
    let rebuilt = app.state.focused().provider.clone().unwrap();
    let (_, text) = ask(&rebuilt, "hi").await;
    assert_eq!(text, "new b", "the pin follows the new gateway");
    assert!(app.state.gateway_view.pins.contains_key(&focused));
}

#[tokio::test]
async fn a_pin_on_a_switched_off_account_is_refused_and_a_later_switch_off_releases_it() {
    let mut app = two_accounts_app().await;
    let b = AccountId::parse("b").unwrap();
    app.gateway
        .apply_edit(ConfigEdit::SetEnabled {
            id: b.clone(),
            enabled: false,
        })
        .unwrap();
    let refused = app.pin_outcome("b").unwrap_err();
    assert!(refused.contains("switched off"), "{refused}");
    assert!(app.state.gateway_view.pins.is_empty());

    // Закреплён живой, потом его выключили: закрепление снимается с заметкой.
    app.gateway
        .apply_edit(ConfigEdit::SetEnabled {
            id: b.clone(),
            enabled: true,
        })
        .unwrap();
    app.pin_outcome("b").unwrap();
    app.gateway
        .apply_edit(ConfigEdit::SetEnabled {
            id: b,
            enabled: false,
        })
        .unwrap();
    app.on_gateway_event_view(&GatewayEvent::ConfigChanged);
    assert!(app.state.gateway_view.pins.is_empty());
    let last = app.state.focused().messages.last().unwrap().content.clone();
    assert!(
        last.contains("Unpinned") && last.contains("switched off"),
        "{last}"
    );
}

#[tokio::test]
async fn models_of_a_pinned_chat_change_its_own_account() {
    let mut app = two_accounts_app().await;
    app.pin_outcome("b").unwrap();
    app.apply_model_switch("brand-new");
    let snapshot = app.gateway.snapshot();
    let model = |id: &str| {
        snapshot
            .accounts
            .iter()
            .find(|a| a.id.as_str() == id)
            .and_then(|a| a.default_model.clone())
    };
    assert_eq!(model("b").as_deref(), Some("brand-new"));
    assert_eq!(
        model("a").as_deref(),
        Some("model-a"),
        "the route's account is untouched"
    );
}

// ── Файл проекта ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_project_file_that_appears_or_vanishes_is_noticed_but_the_first_look_is_not_a_change() {
    let (mut app, _) = app_with_file("stamp").await;
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
    let before = app.state.gateway_view.project.known;
    assert_eq!(before, None, "never looked");
    app.on_project_stamp(None);
    app.on_project_stamp(None);
    assert_eq!(app.state.gateway_view.project.known, Some(None));
    // Файла не было, потом появился: это изменение (раньше терялось).
    app.state.gateway_view.project.checking = true;
    app.on_project_stamp(Some(at));
    assert_eq!(app.state.gateway_view.project.known, Some(Some(at)));
    assert!(
        tokio::time::timeout(Duration::from_secs(60), app.event_rx.recv())
            .await
            .is_ok_and(|e| matches!(e, Some(AppEvent::ConfigReloaded(_)))),
        "a reload was started"
    );
}

#[tokio::test]
async fn a_trust_question_postponed_by_another_modal_comes_back_on_the_tick() {
    let (mut app, _) = app_with_file("trust-later").await;
    app.state.view = View::Chat;
    app.config.source.pending_trust = Some(crate::config::TrustRequest {
        config_path: "p/.pooprusteek/config.toml".into(),
        key: "k".into(),
        hash: "h".into(),
        held_back: vec!["account 'x' sends requests to https://x.example/v1".into()],
        summary: Default::default(),
    });
    app.state.modal = Some(Modal::AccountWizard(Box::default()));
    app.open_trust_dialog_if_needed();
    assert!(
        matches!(app.state.modal, Some(Modal::AccountWizard(_))),
        "not over another modal"
    );
    app.state.modal = None;
    app.tick_gateway_view();
    assert!(
        matches!(app.state.modal, Some(Modal::Trust(_))),
        "asked after the modal closed"
    );
}

#[tokio::test]
async fn a_long_trust_list_cannot_be_trusted_by_tab_and_enter_alone() {
    let (mut app, _) = app_with_file("trust-keys").await;
    let held_back: Vec<String> = (0..40)
        .map(|n| format!("account 'a{n}' sends requests to https://h{n}.example/v1"))
        .collect();
    let request = crate::config::TrustRequest {
        config_path: "p/.pooprusteek/config.toml".into(),
        key: "k".into(),
        hash: "h".into(),
        held_back,
        summary: Default::default(),
    };
    let dialog = Box::new(forms::TrustDialog::new(request));
    app.state.terminal_cols = 80;
    app.state.terminal_rows = 24;
    // Tab к «Trust»: список не прочитан, диалог остаётся на отказе и говорит почему.
    app.handle_trust_key(press(KeyCode::Tab), dialog)
        .await
        .unwrap();
    let Some(Modal::Trust(dialog)) = app.state.modal.take() else {
        panic!("the dialog must stay open");
    };
    assert!(!dialog.confirmed());
    assert!(
        dialog
            .notice
            .as_deref()
            .unwrap_or("")
            .contains("Show every line")
    );
    // Enter на отказе ничего не доверяет и закрывает диалог отказом.
    app.handle_trust_key(press(KeyCode::Enter), dialog)
        .await
        .unwrap();
    assert!(app.state.modal.is_none());
    assert!(app.config.trust.projects.is_empty(), "nothing was trusted");
    assert!(app.state.gateway_view.project.denied.contains("h"));
}

// ── Вид ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_footer_names_the_model_that_answers_when_the_gateway_is_on() {
    let mut app = two_accounts_app().await;
    let model = crate::app::gateway_view::rows::last_decision_model(&app.gateway.snapshot());
    assert_eq!(
        model.as_deref(),
        Some("model-a"),
        "the first step of the route"
    );
    app.pin_outcome("b").unwrap();
    app.refresh_gateway_view();
    let pinned = app
        .state
        .gateway_view
        .pins
        .get(&app.state.conversations.focused_id())
        .unwrap();
    assert_eq!(pinned.account.as_str(), "b");
    assert!(matches!(
        app.gateway.snapshot().accounts[1].status,
        AccountStatus::Ready
    ));
}

// ── Включение и выключение шлюза ────────────────────────────────────────

fn provider_with_key(name: &str, resolved: &str) -> ProviderEntry {
    ProviderEntry {
        api_key: Some(resolved.into()),
        name: name.into(),
        ..lm_entry()
    }
}

async fn enabled_event(app: &mut App) -> Result<GatewayConfig, String> {
    match next_event(app).await {
        AppEvent::GatewayEnabled(result) => *result,
        other => panic!("expected GatewayEnabled, got {other:?}"),
    }
}

/// Ссылка из файла идёт в аккаунт как есть: ключ не теряется и значением в аккаунт не попадает.
#[tokio::test]
async fn enabling_carries_the_reference_of_a_key_as_it_is() {
    let (mut app, path) = app_with_file("enable-refs").await;
    std::fs::write(
        &path,
        "[[providers]]\nname = \"envp\"\nbase_url = \"http://a/v1\"\nmodel = \"m\"\napi_key = \"env:POOP_ENABLE_TEST_A\"\n\n\
         [[providers]]\nname = \"keyp\"\nbase_url = \"http://b/v1\"\nmodel = \"m\"\napi_key = \"keyring:global/team-key\"\n\n\
         [[providers]]\nname = \"cmdp\"\nbase_url = \"http://c/v1\"\nmodel = \"m\"\napi_key = \"cmd:[\\\"echo\\\", \\\"x\\\"]\"\n",
    )
    .unwrap();
    for name in ["envp", "keyp", "cmdp"] {
        let mut entry = provider_with_key(name, "resolved-value-0123456789");
        entry.base_url = format!("http://{name}.invalid/v1");
        app.config.providers.push(entry);
    }
    app.enable_gateway();
    let next = enabled_event(&mut app).await.expect("enabled");
    let secret = |id: &str| {
        next.accounts
            .iter()
            .find(|a| a.id.as_str() == id)
            .and_then(|a| a.secret.as_ref().map(|s| s.as_str().to_string()))
    };
    assert_eq!(secret("envp").as_deref(), Some("env:POOP_ENABLE_TEST_A"));
    assert_eq!(secret("keyp").as_deref(), Some("keyring:global/team-key"));
    assert!(secret("cmdp").unwrap().starts_with("cmd:"));
    assert!(
        next.accounts.iter().all(|a| a.api_key.is_none()),
        "no key value in an account"
    );
    app.on_gateway_enabled(Ok(next));
    assert!(app.config.gateway.enabled);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("env:POOP_ENABLE_TEST_A"), "{text}");
    assert!(
        !text.contains("resolved-value"),
        "the resolved value never reaches the file: {text}"
    );
    assert_eq!(
        text.matches("[[providers]]").count(),
        3,
        "the old entries stay: {text}"
    );
}

/// Ключ, лежащий в файле значением, а хранилище не принимает: включение отменено целиком.
#[tokio::test]
async fn a_key_that_cannot_be_stored_cancels_the_enabling_and_changes_nothing() {
    let (mut app, path) = app_with_file("enable-fail").await;
    std::fs::write(
        &path,
        "[[providers]]\nname = \"plain\"\nbase_url = \"http://a/v1\"\nmodel = \"m\"\napi_key = \"inline-value-0123456789\"\n",
    )
    .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    app.config
        .providers
        .push(provider_with_key("plain", "inline-value-0123456789"));
    app.enable_gateway();
    let result = enabled_event(&mut app).await;
    assert!(result.is_err(), "the test store refuses writes");
    app.on_gateway_enabled(result);
    assert!(
        !app.config.gateway.enabled && app.config.accounts.is_empty(),
        "memory unchanged"
    );
    assert!(!app.gateway.is_enabled());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "the file is untouched"
    );
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(
        notice.error
            && notice.text.contains("was not turned on")
            && notice.text.contains("still work"),
        "{}",
        notice.text
    );
    assert!(app.config.providers.iter().any(|p| p.name == "plain"));
}

#[tokio::test]
async fn an_unreadable_config_file_cancels_the_enabling_too() {
    let (mut app, path) = app_with_file("enable-unreadable").await;
    // На месте файла каталог: прочитать записи нельзя.
    std::fs::create_dir_all(&path).unwrap();
    app.config
        .providers
        .push(provider_with_key("p", "value-0123456789"));
    app.enable_gateway();
    let result = enabled_event(&mut app).await;
    assert!(result.is_err());
    app.on_gateway_enabled(result);
    assert!(!app.config.gateway.enabled);
}

#[tokio::test]
async fn the_gateway_can_be_turned_off_from_the_screen_with_a_question() {
    let mut app = two_accounts_app().await;
    // Старый провайдер, на который чат вернётся.
    app.config.providers.push(lm_entry());
    app.config.active_provider = Some("lm".into());
    app.pin_outcome("a").unwrap();
    app.open_accounts(Focus::Accounts);

    // Вопрос на `X`; всё, кроме `y`, оставляет шлюз включённым.
    app.handle_accounts_key(press(KeyCode::Char('X')))
        .await
        .unwrap();
    assert!(app.state.gateway_view.screen.confirm_disable);
    app.handle_accounts_key(press(KeyCode::Char('n')))
        .await
        .unwrap();
    assert!(app.gateway.is_enabled() && app.config.gateway.enabled);

    app.handle_accounts_key(press(KeyCode::Char('X')))
        .await
        .unwrap();
    app.handle_accounts_key(press(KeyCode::Char('y')))
        .await
        .unwrap();
    let AppEvent::GatewayDisabled(result) = next_event(&mut app).await else {
        panic!("expected GatewayDisabled");
    };
    app.on_gateway_disabled(*result);

    assert!(!app.gateway.is_enabled() && !app.config.gateway.enabled);
    assert_eq!(app.config.accounts.len(), 2, "accounts are kept, not lost");
    assert!(
        app.state.gateway_view.pins.is_empty(),
        "the pin goes with the accounts"
    );
    assert!(
        app.state.focused().provider.is_some(),
        "the open chat is back on a provider"
    );
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(notice.text.contains("Gateway is off"), "{}", notice.text);
}

// ── Экран из инструмента агента ─────────────────────────────────────────

fn trust_request(name: &str) -> crate::config::TrustRequest {
    crate::config::TrustRequest {
        config_path: format!("p/{name}.json").into(),
        key: format!("key-{name}"),
        hash: match name {
            "only" => "h".to_string(),
            other => format!("hash-{other}"),
        },
        held_back: vec![format!("MCP server {name} runs a program")],
        summary: Default::default(),
    }
}

#[tokio::test]
async fn a_screen_started_from_an_agent_tool_turns_nothing_on_and_stores_no_secret() {
    let (mut app, path) = app_with_file("restricted").await;
    app.state.gateway_view.restricted = true;
    app.config
        .providers
        .push(provider_with_key("p", "value-0123456789"));

    app.enable_gateway();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), app.event_rx.recv())
            .await
            .is_err(),
        "no work was started"
    );
    let notice = app.state.gateway_view.screen.notice.clone().unwrap();
    assert!(
        notice.error && notice.text.contains("agent tool"),
        "{}",
        notice.text
    );

    app.open_accounts(Focus::Accounts);
    app.handle_accounts_key(press(KeyCode::Char('g')))
        .await
        .unwrap();
    assert!(
        !app.state.gateway_view.screen.confirm_enable,
        "`g` does not even ask"
    );

    // Мастер: ключ не принимается, пока экран ограничен.
    run_api_wizard(&mut app, "Keyed", "sk-test-canary-0123456789").await;
    let Some(Modal::AccountWizard(wizard)) = &app.state.modal else {
        panic!("the wizard stays open");
    };
    assert!(wizard.error.as_deref().unwrap_or("").contains("agent tool"));
    assert!(provider_names(&path).is_empty());
}

#[tokio::test]
async fn a_restricted_trust_dialog_never_trusts_whatever_the_keys() {
    let (mut app, _) = app_with_file("restricted-trust").await;
    app.state.view = View::Chat;
    app.state.gateway_view.restricted = true;
    app.config.source.pending_trust = Some(trust_request("only"));
    app.open_trust_dialog_if_needed();
    let Some(Modal::Trust(dialog)) = app.state.modal.take() else {
        panic!("the dialog is shown, with its reason");
    };
    assert!(dialog.restricted);
    app.state.terminal_cols = 80;
    app.state.terminal_rows = 24;
    app.handle_trust_key(press(KeyCode::Tab), dialog)
        .await
        .unwrap();
    let Some(Modal::Trust(mut dialog)) = app.state.modal.take() else {
        panic!("still open");
    };
    assert!(!dialog.confirmed());
    assert!(
        dialog
            .notice
            .as_deref()
            .unwrap_or("")
            .contains("agent tool")
    );
    // Даже подделанный выбор не проходит: решение по-прежнему отказ.
    dialog.choice = forms::TrustChoice::Trust;
    app.resolve_trust(*dialog);
    assert!(app.config.trust.projects.is_empty());
    assert!(app.state.gateway_view.project.denied.contains("h"));
}

// ── Диалог доверия: несколько файлов, онбординг, MCP ────────────────────

#[tokio::test]
async fn a_refusal_of_the_first_file_leaves_the_others_to_be_asked() {
    let (mut app, _) = app_with_file("trust-many").await;
    app.state.view = View::Chat;
    app.config.source.pending_trust = Some(trust_request("config"));
    app.config.source.project_mcp = vec![crate::mcp::project::ProjectMcpFile {
        request: trust_request("mcp"),
        text: "{}".into(),
        trusted: false,
    }];
    app.open_trust_dialog_if_needed();
    let Some(Modal::Trust(first)) = app.state.modal.take() else {
        panic!("the first question");
    };
    assert_eq!(first.request.key, "key-config");
    app.handle_trust_key(press(KeyCode::Esc), first)
        .await
        .unwrap();
    let Some(Modal::Trust(second)) = app.state.modal.take() else {
        panic!("the MCP file is asked right after");
    };
    assert_eq!(
        second.request.key, "key-mcp",
        "a refusal does not hide the next file"
    );
    app.handle_trust_key(press(KeyCode::Esc), second)
        .await
        .unwrap();
    assert!(app.state.modal.is_none(), "both were answered");
    assert_eq!(app.state.gateway_view.project.denied.len(), 2);
}

#[tokio::test]
async fn the_trust_question_waits_for_the_onboarding_to_end() {
    let (mut app, _) = app_with_file("trust-onboarding").await;
    app.config.source.pending_trust = Some(trust_request("config"));
    app.state.view = View::Onboarding;
    app.open_trust_dialog_if_needed();
    app.tick_gateway_view();
    assert!(
        app.state.modal.is_none(),
        "its keys would go to the token field"
    );
    app.state.view = View::Chat;
    app.tick_gateway_view();
    assert!(
        matches!(app.state.modal, Some(Modal::Trust(_))),
        "asked once the onboarding is over"
    );
}

async fn mcp_reloaded_within(app: &mut App, wait: Duration) -> bool {
    tokio::time::timeout(wait, async {
        loop {
            if let Some(AppEvent::McpOperationDone { .. }) = app.event_rx.recv().await {
                break;
            }
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn trusting_an_mcp_file_connects_its_servers_without_a_restart() {
    let (mut app, _) = app_with_file("trust-mcp").await;
    let mut fresh = app.config.clone();
    fresh.source.project_mcp = vec![crate::mcp::project::ProjectMcpFile {
        request: trust_request("mcp"),
        text: "{}".into(),
        trusted: true,
    }];
    // Без решения человека серверы не трогаются.
    app.on_config_reloaded(Ok(fresh.clone()));
    assert!(
        !mcp_reloaded_within(&mut app, Duration::from_millis(500)).await,
        "no reload without a decision"
    );
    // Согласие на файл MCP: перезагрузка идёт сама.
    app.state.gateway_view.project.reload_mcp = true;
    app.on_config_reloaded(Ok(fresh));
    assert!(
        mcp_reloaded_within(&mut app, Duration::from_secs(60)).await,
        "the servers are reconnected"
    );
}
