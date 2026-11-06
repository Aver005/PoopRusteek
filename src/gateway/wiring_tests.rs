//! Шлюз и конфиг вместе: настоящий `Config` → `gateway_config` → движок с резолвером секретов → запрос
//! к локальному серверу. Здесь проверяется главное обещание релиза — несколько аккаунтов одного рода,
//! у каждого свой ключ — и привязка секрета к хосту в адаптере.

use super::engine::{EngineDeps, build_gateway, gateway_config};
use super::handle::{CallSource, DecisionQuery, Gateway, GatewayHandle, SkipReason};
use super::types::{Role, Severity, ToolMode};
use crate::config::Config;
use crate::provider::{ChatMessage, CompletionRequest};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Что сервер увидел: путь и заголовок `authorization` каждого запроса.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Локальный сервер: `/a/…` отвечает 429, остальные пути — готовым ответом `ok-from-<путь>`.
async fn server() -> (u16, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    let Some(split) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let length = text[..split]
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0usize);
                    if buffer.len() >= split + 4 + length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buffer).to_string();
                let path = text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let auth = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .starts_with("authorization:")
                            .then(|| l.split_once(':').map_or("", |(_, v)| v.trim()).to_string())
                    })
                    .unwrap_or_default();
                log.lock().unwrap().push((path.clone(), auth));
                let reply = if path.starts_with("/a/") {
                    "HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string()
                } else {
                    // Движок читает ответ потоком (SSE): чанк с текстом, чанк с концом и [DONE].
                    let word = format!("ok-from-{}", path.trim_matches('/').replace('/', "-"));
                    let head =
                        r#""id":"1","object":"chat.completion.chunk","created":0,"model":"m""#;
                    let body = format!(
                        "data: {{{head},\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{word}\"}},\"finish_reason\":null}}]}}\n\ndata: {{{head},\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (port, seen)
}

fn config(text: &str) -> Config {
    crate::config::parse(text).expect("the test config parses")
}

fn engine(config: &Config) -> GatewayHandle {
    let mut deps = EngineDeps::for_config(config);
    // Учёт и подсказки на диск не пишем.
    deps.data_dir = None;
    build_gateway(gateway_config(config), deps)
}

fn query() -> DecisionQuery {
    DecisionQuery {
        role: Role::Main,
        source: CallSource::Chat,
        tool_mode: ToolMode::Prompt,
        input_tokens: None,
        pinned: None,
        sticky: None,
        failed: Vec::new(),
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![ChatMessage::user("hi")],
        tools: Vec::new(),
        model: "m".to_string(),
        temperature: 0.0,
        max_tokens: 16,
        stream: false,
    }
}

fn two_accounts(port: u16, key_a: &str, key_b: &str) -> String {
    format!(
        r#"
        [gateway]
        schema = 1
        enabled = true

        [[accounts]]
        id = "work"
        provider = "openai"
        base_url = "http://127.0.0.1:{port}/a/v1"
        secret = "env:{key_a}"
        model = "m"

        [[accounts]]
        id = "personal"
        provider = "openai"
        base_url = "http://127.0.0.1:{port}/b/v1"
        secret = "env:{key_b}"
        model = "m"

        [[routes]]
        id = "main"
        steps = [{{ account = "work" }}, {{ account = "personal" }}]

        [roles]
        main = "main"
        "#
    )
}

/// Главное обещание релиза: два аккаунта одного рода, у каждого свой ключ из `env:`, и каждый запрос
/// уходит со своим ключом на свой адрес. Первый отвечает 429, второй подхватывает.
#[tokio::test]
async fn two_accounts_of_one_kind_each_send_their_own_key() {
    let (port, seen) = server().await;
    // SAFETY-free: уникальные имена переменных, другие тесты их не читают.
    unsafe {
        std::env::set_var("POOPRUSTEEK_E2E_KEY_WORK", "key-of-work-account-1111");
        std::env::set_var("POOPRUSTEEK_E2E_KEY_PERSONAL", "key-of-personal-acct-2222");
    }
    let config = config(&two_accounts(
        port,
        "POOPRUSTEEK_E2E_KEY_WORK",
        "POOPRUSTEEK_E2E_KEY_PERSONAL",
    ));
    let problems = gateway_config(&config).validate();
    assert!(problems.is_empty(), "validate must be clean: {problems:?}");

    let gateway = engine(&config);
    let provider = gateway
        .provider_for_role(Role::Main, CallSource::Chat)
        .expect("the role has a route");
    let response = provider
        .complete(request())
        .await
        .expect("b answers after a's 429");
    assert!(
        response.content.contains("ok-from-b-v1"),
        "{}",
        response.content
    );

    let seen = seen.lock().unwrap().clone();
    let auth_of = |prefix: &str| {
        seen.iter()
            .find(|(path, _)| path.starts_with(prefix))
            .map(|(_, auth)| auth.clone())
    };
    assert_eq!(
        auth_of("/a/").as_deref(),
        Some("Bearer key-of-work-account-1111"),
        "{seen:?}"
    );
    assert_eq!(
        auth_of("/b/").as_deref(),
        Some("Bearer key-of-personal-acct-2222"),
        "{seen:?}"
    );
    // Чужой ключ ни на один из адресов не ушёл.
    assert!(
        seen.iter()
            .filter(|(path, _)| path.starts_with("/a/"))
            .all(|(_, auth)| !auth.contains("personal")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|(path, _)| path.starts_with("/b/"))
            .all(|(_, auth)| !auth.contains("work")),
        "{seen:?}"
    );
}

/// Секрет, привязанный к другому хосту, не уходит на `base_url` аккаунта, и причина видна в решении.
#[tokio::test]
async fn a_secret_bound_to_another_host_never_reaches_the_base_url() {
    let (port, seen) = server().await;
    unsafe { std::env::set_var("POOPRUSTEEK_E2E_BOUND", "bound-key-value-3333") };
    let text = format!(
        r#"
        [gateway]
        enabled = true
        [[accounts]]
        id = "only"
        provider = "openai"
        base_url = "http://127.0.0.1:{port}/b/v1"
        secret = "env:POOPRUSTEEK_E2E_BOUND"
        model = "m"
        [[routes]]
        id = "main"
        steps = [{{ account = "only" }}]
        [roles]
        main = "main"
        "#
    );
    let mut config = config(&text);
    config.secrets.bindings.insert(
        "env:POOPRUSTEEK_E2E_BOUND".to_string(),
        vec!["https://api.somewhere-else.example".to_string()],
    );
    let gateway = engine(&config);
    let decision = gateway.decide(&query());
    assert!(decision.target.is_none(), "{}", decision.explain());
    let reason = decision
        .skipped
        .iter()
        .find_map(|s| match &s.reason {
            SkipReason::InvalidConfig { message } => Some(message.clone()),
            _ => None,
        })
        .expect("the reason is in the decision");
    assert!(
        reason.contains("not trusted") && reason.contains("127.0.0.1"),
        "{reason}"
    );
    assert!(!reason.contains("bound-key-value"), "{reason}");
    assert!(
        decision.explain().contains("not trusted"),
        "{}",
        decision.explain()
    );

    let provider = gateway
        .provider_for_role(Role::Main, CallSource::Chat)
        .unwrap();
    assert!(provider.complete(request()).await.is_err());
    assert!(
        seen.lock().unwrap().is_empty(),
        "no request left: {:?}",
        seen.lock().unwrap()
    );
}

/// Другая сторона: привязка к хосту аккаунта — ключ уходит.
#[tokio::test]
async fn a_secret_bound_to_the_accounts_own_host_is_sent() {
    let (port, seen) = server().await;
    unsafe { std::env::set_var("POOPRUSTEEK_E2E_BOUND_OK", "bound-key-value-4444") };
    let text = format!(
        r#"
        [gateway]
        enabled = true
        [[accounts]]
        id = "only"
        provider = "openai"
        base_url = "http://127.0.0.1:{port}/b/v1"
        secret = "env:POOPRUSTEEK_E2E_BOUND_OK"
        model = "m"
        [[routes]]
        id = "main"
        steps = [{{ account = "only" }}]
        [roles]
        main = "main"
        "#
    );
    let mut config = config(&text);
    config.secrets.bindings.insert(
        "env:POOPRUSTEEK_E2E_BOUND_OK".to_string(),
        vec!["http://127.0.0.1".to_string()],
    );
    let gateway = engine(&config);
    let provider = gateway
        .provider_for_role(Role::Main, CallSource::Chat)
        .unwrap();
    provider
        .complete(request())
        .await
        .expect("the bound host gets the key");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].1, "Bearer bound-key-value-4444");
}

/// Привязка действует и после правки `base_url`: адрес берётся из аккаунта, а не из снимка при старте.
#[test]
fn the_host_check_uses_the_accounts_current_address() {
    use crate::gateway::engine::SecretResolver as EngineResolver;
    unsafe { std::env::set_var("POOPRUSTEEK_E2E_MOVED", "moved-key-value-5555") };
    let mut config = config(
        r#"
        [[accounts]]
        id = "moved"
        provider = "openai"
        base_url = "https://api.good.example/v1"
        secret = "env:POOPRUSTEEK_E2E_MOVED"
        model = "m"
        "#,
    );
    config.secrets.bindings.insert(
        "env:POOPRUSTEEK_E2E_MOVED".to_string(),
        vec!["api.good.example".to_string()],
    );
    let resolver = config.secret_resolver();
    let account = config.accounts[0].clone();
    let reference = account.secret.clone().unwrap();
    assert!(resolver.resolve_for(&reference, &account).is_ok());
    // Адрес поменяли в панели: резолвер снимок не обновлялся, но проверяет текущий адрес аккаунта.
    let mut moved = account;
    moved.base_url = Some("https://evil.example/v1".to_string());
    let error = resolver
        .resolve_for(&reference, &moved)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("not trusted") && error.contains("evil.example"),
        "{error}"
    );
}

/// Запись `[[providers]]` без модели: перенос и `from_legacy` её не теряют, шлюз при `enabled = true`
/// объясняет причину, а прежняя работа при `enabled = false` не меняется.
#[tokio::test]
async fn a_provider_without_a_model_is_kept_explained_and_harmless_while_the_gateway_is_off() {
    let mut config = config(
        r#"
        [[providers]]
        name = "lmstudio"
        base_url = "http://127.0.0.1:1/v1"
        "#,
    );
    let (legacy, _) = super::types::GatewayConfig::from_legacy(&config);
    assert_eq!(legacy.accounts.len(), 1, "the entry is not lost");
    assert!(legacy.accounts[0].model.is_none());

    // enabled = false: шлюз ничего не делает, прежний провайдер собирается как раньше.
    config.active_provider = Some("lmstudio".to_string());
    let off = engine(&config);
    assert!(!off.is_enabled());
    assert!(crate::gateway::handle::main_provider(&config, Some(&off)).is_some());

    // enabled = true с маршрутом через эту запись: причина названа и в проверке, и в решении.
    config.gateway.enabled = true;
    config.accounts = legacy.accounts;
    config.routes =
        vec![toml::from_str("id = \"main\"\nsteps = [{ account = \"lmstudio\" }]\n").unwrap()];
    config.roles = toml::from_str("main = \"main\"\n").unwrap();
    let problems = gateway_config(&config).validate();
    assert!(
        problems
            .iter()
            .any(|p| p.severity == Severity::Error && p.message.contains("has no model")),
        "{problems:?}"
    );
    let on = engine(&config);
    let decision = on.decide(&query());
    assert!(decision.target.is_none());
    assert!(
        decision.explain().contains("no model") || decision.explain().contains("route is off"),
        "{}",
        decision.explain()
    );
}
