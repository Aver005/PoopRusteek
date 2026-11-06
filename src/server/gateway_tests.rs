//! `/serve` через шлюз: настоящий локальный сервер с одной стороны, настоящий «апстрим» на TCP с другой.
//! Проверяется то, что видит клиент: коды, поток, заголовки, расход, каталог, статус.

use super::ServerSettings;
use super::testkit::{
    Script, Stand, Upstream, engine, eventually, settings, start, start_with, upstream,
};
use crate::gateway::handle::{AccountStatus, LedgerQuery};
use crate::gateway::testkit::{Act, FakeBackend};
use std::sync::Arc;

/// Подписки CLI в тестах отвечает подставной адаптер: так видно, что его ни разу не вызвали.
struct CliFactory(Arc<FakeBackend>);

impl crate::gateway::engine::BackendFactory for CliFactory {
    fn build(
        &self,
        account: &crate::gateway::types::Account,
        _secrets: &dyn crate::gateway::engine::SecretResolver,
    ) -> Option<Result<Arc<dyn crate::gateway::backend::Backend>, String>> {
        account
            .provider
            .cli_tool()
            .map(|_| Ok(Arc::clone(&self.0) as Arc<dyn crate::gateway::backend::Backend>))
    }
}

fn text_stream(chunks: &[&'static str]) -> Script {
    Script::Stream {
        chunks: chunks.to_vec(),
        usage: None,
        headers: Vec::new(),
    }
}

/// Два API-аккаунта `a` и `b`, маршрут `main` и (по желанию) подписка CLI `sub`.
fn config(port: u16, with_cli: bool) -> String {
    let cli = if with_cli {
        r#"
        [[accounts]]
        id = "sub"
        provider = "cli:claude"
        home = "/tmp/pooprusteek-test-claude-home"
        model = "opus"

        [[accounts]]
        id = "oc"
        provider = "cli:opencode"
        home = "/tmp/pooprusteek-test-opencode-home"
        model = "kimi"

        [[accounts]]
        id = "cx"
        provider = "cli:codex"
        home = "/tmp/pooprusteek-test-codex-home"
        model = "gpt"

        [[routes]]
        id = "subscription"
        steps = [{ account = "sub" }]
        "#
    } else {
        ""
    };
    format!(
        r#"
        [gateway]
        schema = 1
        enabled = true

        [[accounts]]
        id = "a"
        provider = "openai"
        base_url = "http://127.0.0.1:{port}/a/v1"
        secret = "env:POOPRUSTEEK_SERVE_TEST_KEY_A"
        model = "m"

        [[accounts]]
        id = "b"
        provider = "openai"
        base_url = "http://127.0.0.1:{port}/b/v1"
        secret = "env:POOPRUSTEEK_SERVE_TEST_KEY_B"
        model = "m"
        {cli}
        [[routes]]
        id = "main"
        steps = [{{ account = "a" }}, {{ account = "b" }}]
        "#
    )
}

const KEY_A: &str = "serve-test-key-a-1111aaaa";
const KEY_B: &str = "serve-test-key-b-2222bbbb";

fn set_keys() {
    // Имена уникальны для этого файла: других читателей у переменных нет.
    unsafe {
        std::env::set_var("POOPRUSTEEK_SERVE_TEST_KEY_A", KEY_A);
        std::env::set_var("POOPRUSTEEK_SERVE_TEST_KEY_B", KEY_B);
    }
}

async fn stand(scripts: Vec<(&'static str, Vec<Script>)>) -> (Stand, Upstream) {
    set_keys();
    let up = upstream(scripts).await;
    let gateway = engine(&config(up.port, false), Vec::new());
    (start(gateway, None).await, up)
}

async fn post(stand: &Stand, model: &str, stream: bool) -> reqwest::Response {
    post_body(
        stand,
        serde_json::json!({
            "model": model,
            "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        }),
    )
    .await
}

async fn post_body(stand: &Stand, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", stand.base))
        .json(&body)
        .send()
        .await
        .expect("the request reaches the server")
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Содержимое всех SSE-кадров `data:` ответа.
fn frames(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(str::to_string)
        .collect()
}

fn text_of(body: &str) -> String {
    frames(body)
        .iter()
        .filter_map(|f| serde_json::from_str::<serde_json::Value>(f).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

// ── Fallback ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_429_from_the_first_account_is_answered_by_the_second_before_any_byte() {
    let (stand, up) = stand(vec![
        ("/a/", vec![Script::RateLimited]),
        ("/b/", vec![text_stream(&["Hello ", "from b"])]),
    ])
    .await;
    let response = post(&stand, "auto/main", true).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-pooprusteek-account").as_deref(),
        Some("b")
    );
    assert_eq!(
        header(&response, "x-pooprusteek-model").as_deref(),
        Some("m")
    );
    assert_eq!(
        header(&response, "x-pooprusteek-route").as_deref(),
        Some("main")
    );
    assert_eq!(
        header(&response, "x-pooprusteek-fallback").as_deref(),
        Some("a")
    );
    assert_eq!(
        header(&response, "x-pooprusteek-fallback-reason").as_deref(),
        Some("rate_limit")
    );
    let body = response.text().await.unwrap();
    assert_eq!(text_of(&body), "Hello from b");
    assert!(body.contains("data: [DONE]"), "{body}");
    assert_eq!((up.hits("/a/"), up.hits("/b/")), (1, 1));
}

#[tokio::test]
async fn after_the_first_byte_the_error_stays_in_the_stream_and_the_other_account_is_not_mixed_in()
{
    let (stand, up) = stand(vec![
        ("/a/", vec![Script::Break("Part")]),
        ("/b/", vec![text_stream(&["from b"])]),
    ])
    .await;
    let response = post(&stand, "auto/main", true).await;
    assert_eq!(
        response.status(),
        200,
        "the status left with the first byte"
    );
    assert_eq!(
        header(&response, "x-pooprusteek-account").as_deref(),
        Some("a")
    );
    assert_eq!(header(&response, "x-pooprusteek-fallback"), None);
    let body = response.text().await.unwrap();
    assert!(text_of(&body).starts_with("Part"), "{body}");
    assert!(!text_of(&body).contains("from b"), "{body}");
    assert!(body.contains("\"error\""), "an honest error frame: {body}");
    assert!(!body.contains("data: [DONE]"), "{body}");
    assert_eq!(up.hits("/b/"), 0, "the second account was never asked");
}

#[tokio::test]
async fn when_every_account_fails_before_a_byte_the_client_gets_a_real_status() {
    let (stand, _up) = stand(vec![
        ("/a/", vec![Script::RateLimited]),
        ("/b/", vec![Script::RateLimited]),
    ])
    .await;
    let response = post(&stand, "auto/main", true).await;
    assert_eq!(response.status(), 429);
    assert!(header(&response, "retry-after").is_some());
    let envelope: serde_json::Value = response.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "rate_limit_exceeded");
}

#[tokio::test]
async fn a_blocking_request_falls_back_too_and_reports_the_same_headers() {
    let (stand, _up) = stand(vec![
        ("/a/", vec![Script::RateLimited]),
        ("/b/", vec![text_stream(&["plain"])]),
    ])
    .await;
    let response = post(&stand, "auto/main", false).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-pooprusteek-account").as_deref(),
        Some("b")
    );
    assert_eq!(
        header(&response, "x-pooprusteek-fallback").as_deref(),
        Some("a")
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "plain");
}

// ── Каталог и 404 ───────────────────────────────────────────────────────

#[tokio::test]
async fn cli_subscriptions_are_neither_listed_nor_accepted() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let cli = FakeBackend::new(vec![Act::Reply("subscription answer")]);
    let gateway = engine(
        &config(up.port, true),
        vec![Arc::new(CliFactory(Arc::clone(&cli)))],
    );
    // Подписка собрана и готова: скрывает её именно правило раздачи, а не недоступность.
    let ready = gateway
        .snapshot()
        .accounts
        .iter()
        .any(|a| a.id.as_str() == "sub" && a.status == AccountStatus::Ready);
    assert!(ready, "the test needs a working subscription account");
    let stand = start(gateway, None).await;

    let listed: serde_json::Value = reqwest::get(format!("{}/v1/models", stand.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"a/m") && ids.contains(&"auto/main"),
        "{ids:?}"
    );
    assert!(
        !ids.iter()
            .any(|id| ["sub", "oc", "cx"].iter().any(|p| id.starts_with(p))
                || id.contains("subscription")),
        "no CLI model or route may be advertised: {ids:?}"
    );

    for model in ["sub/opus", "oc/kimi", "cx/gpt", "auto/subscription", "sub"] {
        let response = post(&stand, model, false).await;
        assert_eq!(response.status(), 404, "{model}");
        let envelope: serde_json::Value = response.json().await.unwrap();
        assert_eq!(envelope["error"]["code"], "model_not_found", "{model}");
    }
    assert_eq!(cli.call_count(), 0, "the subscription was never called");
}

#[tokio::test]
async fn unknown_accounts_routes_and_models_are_honest_404s() {
    let (stand, up) = stand(vec![("/a/", vec![text_stream(&["x"])])]).await;
    for model in [
        "auto/unknown",
        "auto/",
        "nosuch/m",
        "a/not-a-listed-model",
        "a/",
        "m",
        "a",
        "a/../b/m",
        "a/m x",
        "a/m?base_url=http://evil",
    ] {
        let response = post(&stand, model, false).await;
        assert_eq!(response.status(), 404, "{model}");
    }
    assert!(
        up.seen.lock().unwrap().is_empty(),
        "nothing reached an upstream"
    );
    let ok = post(&stand, "a/m", false).await;
    assert_eq!(ok.status(), 200);
}

// ── Расход ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn usage_reaches_a_client_that_asked_for_it_and_the_ledger_counts_it_once() {
    let (stand, _up) = stand(vec![(
        "/a/",
        vec![Script::Stream {
            chunks: vec!["Hel", "lo"],
            usage: Some((11, 7)),
            headers: Vec::new(),
        }],
    )])
    .await;
    let asked = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", stand.base))
        .json(&serde_json::json!({
            "model": "a/m",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let all = frames(&asked);
    let with_usage: Vec<&String> = all.iter().filter(|f| f.contains("\"usage\"")).collect();
    assert_eq!(with_usage.len(), 1, "exactly one usage frame: {asked}");
    let usage: serde_json::Value = serde_json::from_str(with_usage[0]).unwrap();
    assert_eq!(usage["usage"]["prompt_tokens"], 11);
    assert_eq!(usage["usage"]["completion_tokens"], 7);
    assert_eq!(usage["choices"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        all.last().map(String::as_str),
        Some("[DONE]"),
        "usage goes before [DONE]"
    );
    let position = all.iter().position(|f| f.contains("\"usage\"")).unwrap();
    assert!(
        all[..position].iter().any(|f| f.contains("\"stop\"")),
        "after the finish: {asked}"
    );

    let ledger = stand.cell.get().ledger();
    let counted = eventually(|| {
        ledger
            .totals(&LedgerQuery::default())
            .iter()
            .map(|r| r.calls)
            .sum::<u64>()
            == 1
    })
    .await;
    assert!(counted, "{:?}", ledger.totals(&LedgerQuery::default()));
    let rows = ledger.totals(&LedgerQuery::default());
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].prompt_tokens, rows[0].completion_tokens), (11, 7));
    assert!(!rows[0].any_estimated, "the numbers are the server's own");

    // Тот же ответ без просьбы: клиенту расход не уходит, учёт растёт на ту же величину.
    let plain = post(&stand, "a/m", true).await.text().await.unwrap();
    assert!(!plain.contains("\"usage\""), "{plain}");
    let twice = eventually(|| {
        let rows = ledger.totals(&LedgerQuery::default());
        rows.iter().map(|r| r.calls).sum::<u64>() == 2
            && rows.iter().map(|r| r.prompt_tokens).sum::<u64>() == 22
    })
    .await;
    assert!(twice, "{:?}", ledger.totals(&LedgerQuery::default()));
}

#[tokio::test]
async fn a_client_that_leaves_mid_stream_cancels_the_upstream_without_cooling_the_account() {
    let (stand, up) = stand(vec![(
        "/a/",
        vec![Script::Hang("Hi"), text_stream(&["after"])],
    )])
    .await;
    let mut response = post(&stand, "a/m", true).await;
    assert_eq!(response.status(), 200);
    let first = response.chunk().await.unwrap().expect("the first bytes");
    assert!(String::from_utf8_lossy(&first).contains("Hi"));
    drop(response);

    assert!(
        eventually(|| up.closed.load(std::sync::atomic::Ordering::SeqCst) == 1).await,
        "the upstream connection was closed by us"
    );
    let ledger = stand.cell.get().ledger();
    assert!(
        eventually(|| ledger
            .totals(&LedgerQuery::default())
            .iter()
            .map(|r| r.calls)
            .sum::<u64>()
            == 1)
        .await,
        "the cancelled call is counted once: {:?}",
        ledger.totals(&LedgerQuery::default())
    );
    let status = stand
        .cell
        .get()
        .snapshot()
        .accounts
        .iter()
        .find(|a| a.id.as_str() == "a")
        .map(|a| a.status.clone());
    assert_eq!(
        status,
        Some(AccountStatus::Ready),
        "a leaving client is not the account's fault"
    );

    // Следующий запрос идёт на тот же аккаунт без ожидания.
    let again = post(&stand, "a/m", false).await;
    assert_eq!(again.status(), 200);
    assert_eq!(header(&again, "x-pooprusteek-fallback"), None);
}

// ── Секреты и недоверие к клиенту ───────────────────────────────────────

#[tokio::test]
async fn upstream_auth_errors_and_keys_never_reach_the_client() {
    let (stand, _up) = stand(vec![("/a/", vec![Script::RejectsKey])]).await;
    let response = post(&stand, "a/m", false).await;
    assert_eq!(response.status(), 502);
    let headers = format!("{:?}", response.headers());
    let text = response.text().await.unwrap();
    for leaked in [KEY_A, KEY_B, "127.0.0.1:", "/a/v1"] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
        assert!(!headers.contains(leaked), "{leaked} in {headers}");
    }
    assert!(text.contains("upstream_auth"), "{text}");
}

#[tokio::test]
async fn a_client_cannot_steer_the_upstream_address_or_credentials() {
    let (stand, up) = stand(vec![("/a/", vec![text_stream(&["ok"])])]).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", stand.base))
        .header("authorization", "Bearer client-supplied-key")
        .header("x-api-key", "client-header-key")
        .header("openai-organization", "evil-org")
        .json(&serde_json::json!({
            "model": "a/m",
            "base_url": "http://127.0.0.1:1/evil",
            "api_key": "client-body-key",
            "headers": {"x-evil": "1"},
            "cli_args": ["--dangerously-skip-permissions"],
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = up.last("/a/").expect("the account's own address was used");
    assert_eq!(seen.auth, format!("Bearer {KEY_A}"));
    assert!(
        !seen.body.contains("client-body-key") && !seen.body.contains("evil"),
        "{}",
        seen.body
    );
}

// ── Статус ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_status_endpoint_needs_the_api_key_and_shows_ages_sources_and_decisions() {
    set_keys();
    let up = upstream(vec![(
        "/a/",
        vec![Script::Stream {
            chunks: vec!["x"],
            usage: None,
            headers: vec![
                ("x-ratelimit-limit-requests", "100"),
                ("x-ratelimit-remaining-requests", "40"),
            ],
        }],
    )])
    .await;
    let stand = start(
        engine(&config(up.port, false), Vec::new()),
        Some("server-key"),
    )
    .await;
    let client = reqwest::Client::new();
    let _ = client
        .post(format!("{}/v1/chat/completions", stand.base))
        .bearer_auth("server-key")
        .json(&serde_json::json!({"model": "auto/main", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();

    for path in ["/v1/pooprusteek/status", "/pooprusteek/status"] {
        let anonymous = client
            .get(format!("{}{path}", stand.base))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401, "{path}");
        let wrong = client
            .get(format!("{}{path}", stand.base))
            .bearer_auth("not-the-key")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401, "{path}");
    }
    let status = client
        .get(format!("{}/v1/pooprusteek/status", stand.base))
        .bearer_auth("server-key")
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let text = status.text().await.unwrap();
    for leaked in [KEY_A, KEY_B, "server-key", "127.0.0.1:", "/a/v1", "http://"] {
        assert!(!text.contains(leaked), "{leaked} in {text}");
    }
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    let account = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "a")
        .expect("account a");
    assert_eq!(account["status"]["state"], "ready");
    assert_eq!(account["limits"]["source"], "header");
    assert!(account["limits"]["age_secs"].is_u64(), "{account}");
    assert_eq!(account["limits"]["windows"][0]["remaining"], 40.0);
    assert!(!body["decisions"].as_array().unwrap().is_empty(), "{body}");
    assert!(
        body["decisions"][0]["explain"]
            .as_str()
            .unwrap()
            .contains('a')
    );
}

#[tokio::test]
async fn without_a_server_key_the_status_is_trimmed_and_only_on_loopback() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let gateway = engine(&config(up.port, false), Vec::new());
    let loopback = start(gateway, None).await;
    let _ = post(&loopback, "a/m", false).await;
    let body: serde_json::Value = reqwest::get(format!("{}/v1/pooprusteek/status", loopback.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        body.get("decisions").is_none() && body.get("problems").is_none(),
        "{body}"
    );
    let account = &body["accounts"][0];
    assert!(
        account.get("limits").is_none() && account.get("label").is_none(),
        "{account}"
    );
    assert!(account["status"].get("reason").is_none(), "{account}");
}

#[tokio::test]
async fn cors_never_exposes_the_routing_headers_to_a_foreign_site() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let gateway = engine(&config(up.port, false), Vec::new());
    let keyed = start(gateway.clone(), Some("server-key")).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", keyed.base))
        .bearer_auth("server-key")
        .header("origin", "https://evil.example")
        .json(&serde_json::json!({"model": "a/m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(header(&response, "x-pooprusteek-account").is_some());
    assert_eq!(header(&response, "access-control-expose-headers"), None);

    let open = start(gateway, None).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", open.base))
        .header("origin", "https://evil.example")
        .json(&serde_json::json!({"model": "a/m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        None,
        "no key, no CORS"
    );
    assert_eq!(header(&response, "access-control-expose-headers"), None);
}

// ── Живой шлюз и прежний путь ───────────────────────────────────────────

#[tokio::test]
async fn a_gateway_that_is_still_building_asks_the_client_to_wait_and_then_works() {
    use crate::gateway::pending::{PendingGateway, PendingHandle};
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["ready now"])])]).await;
    let text = config(up.port, false);
    let parsed = crate::config::parse(&text).unwrap();
    let pending = PendingGateway::new(crate::gateway::engine::gateway_config(&parsed));
    let stand = start(Arc::new(PendingHandle(Arc::clone(&pending))), None).await;

    let waiting = post(&stand, "a/m", false).await;
    assert_eq!(waiting.status(), 503);
    assert_eq!(header(&waiting, "retry-after").as_deref(), Some("2"));
    let envelope: serde_json::Value = waiting.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "gateway_starting");

    // Приложение подменяет шлюз на собранный: сервер подхватывает без перезапуска.
    let built = engine(&text, Vec::new());
    pending.fulfil(Arc::clone(&built));
    stand.cell.set(built);
    let ready = post(&stand, "a/m", false).await;
    assert_eq!(ready.status(), 200);
}

#[tokio::test]
async fn the_server_follows_a_replaced_gateway_on_the_next_request() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let text = config(up.port, false);
    let stand = start(engine(&text, Vec::new()), None).await;
    assert_eq!(post(&stand, "a/m", false).await.status(), 200);
    // Правка конфига шлюза видна сразу: аккаунт выключили — модель исчезла.
    let without_a = text.replace("id = \"a\"\n", "id = \"a\"\n        enabled = false\n");
    stand.cell.set(engine(&without_a, Vec::new()));
    assert_eq!(post(&stand, "a/m", false).await.status(), 404);
}

#[tokio::test]
async fn health_does_not_list_gateway_accounts_without_a_key() {
    let (stand, _up) = stand(vec![]).await;
    let health: serde_json::Value = reqwest::get(format!("{}/health", stand.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        health["models"].as_array().map(Vec::len),
        Some(0),
        "{health}"
    );
}

/// Прежний путь при выключенном шлюзе: список по записям, поток сразу 200, без заголовков маршрута.
#[tokio::test]
async fn with_the_gateway_off_the_old_path_is_unchanged() {
    let up = upstream(vec![("/legacy/", vec![Script::RateLimited])]).await;
    let mut s = settings(
        Arc::new(crate::gateway::handle::DisabledGateway::new()),
        None,
        "127.0.0.1",
    );
    s.entries = vec![crate::config::ProviderEntry {
        name: "old".to_string(),
        base_url: format!("http://127.0.0.1:{}/legacy", up.port),
        api_key: None,
        model: "m".to_string(),
        protocol: crate::config::ProviderProtocol::Openai,
        tools: Default::default(),
    }];
    let stand = start_with(s).await;
    let listed: serde_json::Value = reqwest::get(format!("{}/v1/models", stand.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["data"][0]["id"], "old/m");
    let response = post(&stand, "old/m", true).await;
    assert_eq!(
        response.status(),
        200,
        "old behaviour: the error travels inside the stream"
    );
    assert_eq!(header(&response, "x-pooprusteek-account"), None);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"error\""), "{body}");
    // Статус-эндпоинт без шлюза нечего показывать.
    let status = reqwest::get(format!("{}/v1/pooprusteek/status", stand.base))
        .await
        .unwrap();
    assert_eq!(status.status(), 404);
}

// ── ACP и прокси: тот же путь сборки, что в приложении ──────────────────

#[tokio::test]
async fn the_acp_provider_waits_for_the_gateway_to_be_built_and_then_answers() {
    use crate::gateway::handle::GatewayHandle;
    use crate::gateway::pending::{PendingGateway, PendingHandle};
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["ready ", "now"])])]).await;
    let text = format!("{}\n[roles]\nmain = \"main\"\n", config(up.port, false));
    let parsed = crate::config::parse(&text).unwrap();
    let pending = PendingGateway::new(crate::gateway::engine::gateway_config(&parsed));
    let handle: GatewayHandle = Arc::new(PendingHandle(Arc::clone(&pending)));

    // Именно так ACP берёт провайдера: сборка ещё идёт, провайдер уже есть.
    let provider = crate::gateway::handle::main_provider(&parsed, Some(&handle))
        .expect("a provider is handed out while the gateway is being built");
    let request = crate::provider::CompletionRequest {
        messages: vec![crate::provider::ChatMessage::user("hi")],
        tools: Vec::new(),
        model: "x".to_string(),
        temperature: 0.0,
        max_tokens: 16,
        stream: false,
    };
    let asking = tokio::spawn({
        let provider = Arc::clone(&provider);
        async move { provider.complete(request).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!asking.is_finished(), "the request waits for the build");

    pending.fulfil(engine(&text, Vec::new()));
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), asking)
        .await
        .expect("the request resumes")
        .unwrap()
        .expect("and is answered");
    assert_eq!(reply.content, "ready now");
}

// ── Каталог аккаунтов шлюза без записей `[[providers]]` ─────────────────

#[tokio::test]
async fn every_gateway_account_gets_its_real_models_without_a_providers_entry() {
    set_keys();
    let up = upstream(vec![
        (
            "/a/v1/models",
            vec![Script::Models(vec!["m", "big-model", "embed-1"])],
        ),
        ("/b/v1/models", vec![Script::Models(vec!["m"])]),
        ("/a/", vec![text_stream(&["x"])]),
        ("/b/", vec![text_stream(&["y"])]),
    ])
    .await;
    let cli = FakeBackend::new(vec![Act::Reply("never")]);
    let gateway = engine(
        &config(up.port, true),
        vec![Arc::new(CliFactory(Arc::clone(&cli)))],
    );
    let stand = start(gateway.clone(), None).await;
    let listed = |stand: &Stand| {
        let base = stand.base.clone();
        async move {
            let body: serde_json::Value = reqwest::get(format!("{base}/v1/models"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        }
    };
    assert!(!listed(&stand).await.contains(&"a/big-model".to_string()));
    assert_eq!(post(&stand, "a/big-model", false).await.status(), 404);

    // Так же, как приложение и прокси: кэш подключён к шлюзу, записей `[[providers]]` нет.
    stand.models.set_gateway(gateway);
    let outcome = stand.models.refresh(&[], 60_000, true).await;
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);

    let ids = listed(&stand).await;
    for expected in ["a/m", "a/big-model", "a/embed-1", "b/m", "auto/main"] {
        assert!(ids.contains(&expected.to_string()), "{expected} in {ids:?}");
    }
    assert!(
        !ids.iter()
            .any(|id| id.starts_with("sub/") || id.starts_with("oc/") || id.starts_with("cx/")),
        "CLI models stay out of /serve even though the catalog keeps them: {ids:?}"
    );
    // Каталог хранит и подписки: TUI выбирает из него модель.
    assert!(stand.models.snapshot().contains_key("sub"));
    assert_eq!(post(&stand, "a/big-model", false).await.status(), 200);
    assert_eq!(
        post(&stand, "a/not-served-by-anyone", false).await.status(),
        404
    );
    assert_eq!(post(&stand, "sub/listed", false).await.status(), 404);
    assert_eq!(cli.call_count(), 0);
}

#[tokio::test]
async fn a_refresh_during_the_build_leaves_the_cache_alone_and_a_ready_gateway_fills_it() {
    use crate::gateway::pending::{PendingGateway, PendingHandle};
    set_keys();
    let up = upstream(vec![("/a/v1/models", vec![Script::Models(vec!["m"])])]).await;
    let text = config(up.port, false);
    let parsed = crate::config::parse(&text).unwrap();
    let pending = PendingGateway::new(crate::gateway::engine::gateway_config(&parsed));
    let models = crate::provider::model_cache::ProviderModelCache::empty_for_tests();
    models.set_gateway(Arc::new(PendingHandle(Arc::clone(&pending))));
    models.refresh(&[], 0, true).await;
    assert!(
        models.snapshot().is_empty(),
        "nothing to fetch while building"
    );

    models.set_gateway(engine(&text, Vec::new()));
    models.refresh(&[], 0, true).await;
    assert_eq!(models.snapshot().get("a").map(Vec::len), Some(1));
}

// ── `/embeddings` и `/rerank` за шлюзом ─────────────────────────────────

#[tokio::test]
async fn embeddings_go_to_the_named_account_with_its_own_key_and_address() {
    let (stand, up) = stand(vec![
        (
            "/a/v1/embeddings",
            vec![Script::Json(
                r#"{"object":"list","data":[{"embedding":[0.5]}]}"#,
            )],
        ),
        ("/a/", vec![text_stream(&["x"])]),
    ])
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", stand.base))
        .json(&serde_json::json!({"model": "a/m", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["embedding"][0], 0.5);
    let seen = up.last("/a/v1/embeddings").expect("the account was called");
    assert_eq!(seen.auth, format!("Bearer {KEY_A}"));
    let sent: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(
        sent["model"], "m",
        "the account's own model name goes upstream"
    );

    // Маршрут выбирает аккаунт на каждый запрос, поэтому для эмбеддингов нужен адрес аккаунта.
    let route = reqwest::Client::new()
        .post(format!("{}/v1/rerank", stand.base))
        .json(&serde_json::json!({"model": "auto/main", "query": "q", "documents": ["d"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(route.status(), 400);
    let envelope: serde_json::Value = route.json().await.unwrap();
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("<account>/<model>"),
        "{envelope}"
    );
    let unknown = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", stand.base))
        .json(&serde_json::json!({"model": "nosuch/m", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    assert_eq!(
        up.hits("/a/v1/embeddings"),
        1,
        "refusals never reached an upstream"
    );
}

#[tokio::test]
async fn embeddings_never_reach_a_subscription_account() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let cli = FakeBackend::new(vec![Act::Reply("never")]);
    let gateway = engine(
        &config(up.port, true),
        vec![Arc::new(CliFactory(Arc::clone(&cli)))],
    );
    let stand = start(gateway, None).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", stand.base))
        .json(&serde_json::json!({"model": "sub/opus", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(cli.call_count(), 0);
}

// ── Ошибки сети, сборка шлюза ───────────────────────────────────────────

/// Свободный порт, на котором никто не слушает.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn a_network_failure_never_shows_the_accounts_address() {
    set_keys();
    let port = closed_port();
    for stream in [false, true] {
        // Свой шлюз на каждый заход: после первого отказа аккаунт остывает.
        let stand = start(engine(&config(port, false), Vec::new()), None).await;
        let response = post(&stand, "a/m", stream).await;
        assert!(
            response.status().is_server_error(),
            "stream={stream}: {}",
            response.status()
        );
        let text = response.text().await.unwrap();
        for leaked in ["127.0.0.1", &port.to_string(), "/a/v1", "http://"] {
            assert!(
                !text.contains(leaked),
                "stream={stream}: {leaked} in {text}"
            );
        }
    }
}

#[tokio::test]
async fn the_catalog_during_a_build_is_a_503_and_a_failed_build_says_so_without_retry() {
    use crate::gateway::pending::{PendingGateway, PendingHandle};
    set_keys();
    let text = config(closed_port(), false);
    let parsed = crate::config::parse(&text).unwrap();
    let pending = PendingGateway::new(crate::gateway::engine::gateway_config(&parsed));
    let stand = start(Arc::new(PendingHandle(Arc::clone(&pending))), None).await;
    let models = || async {
        reqwest::get(format!("{}/v1/models", stand.base))
            .await
            .unwrap()
    };

    let starting = models().await;
    assert_eq!(starting.status(), 503);
    assert_eq!(header(&starting, "retry-after").as_deref(), Some("2"));

    pending.fail("the secret env:MISSING is not set".to_string());
    for response in [models().await, post(&stand, "a/m", false).await] {
        assert_eq!(response.status(), 503);
        assert_eq!(
            header(&response, "retry-after"),
            None,
            "retrying cannot help"
        );
        let envelope: serde_json::Value = response.json().await.unwrap();
        assert_eq!(envelope["error"]["code"], "gateway_failed");
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("env:MISSING"),
            "{envelope}"
        );
    }
}

#[tokio::test]
async fn an_error_response_does_not_claim_that_an_account_answered() {
    let (stand, _up) = stand(vec![
        ("/a/", vec![Script::RateLimited]),
        ("/b/", vec![Script::RateLimited]),
    ])
    .await;
    let response = post(&stand, "auto/main", false).await;
    assert_eq!(response.status(), 429);
    assert_eq!(header(&response, "x-pooprusteek-account"), None);
}

// ── Host (DNS rebinding) и доступ к статусу ─────────────────────────────

async fn get_with_host(stand: &Stand, path: &str, host: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{}{path}", stand.base))
        .header("host", host)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_foreign_host_header_is_refused_on_a_loopback_server() {
    let (stand, _up) = stand(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let port = stand.base.rsplit(':').next().unwrap().to_string();
    for path in ["/health", "/v1/models", "/v1/pooprusteek/status"] {
        for host in [
            "evil.example".to_string(),
            format!("evil.example:{port}"),
            "127.0.0.1:1".to_string(),
        ] {
            let response = get_with_host(&stand, path, &host).await;
            assert_eq!(response.status(), 403, "{path} with Host {host}");
        }
        for host in [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ] {
            let response = get_with_host(&stand, path, &host).await;
            assert_eq!(response.status(), 200, "{path} with Host {host}");
        }
    }
    // Запрос чата с чужим именем тоже не доходит до апстрима.
    let chat = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", stand.base))
        .header("host", "evil.example")
        .json(&serde_json::json!({"model": "a/m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 403);
}

#[tokio::test]
async fn configured_names_are_allowed_and_the_key_is_still_checked() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let mut s = settings(
        engine(&config(up.port, false), Vec::new()),
        Some("server-key"),
        "127.0.0.1",
    );
    s.allowed_hosts = vec!["proxy.lan".to_string()];
    let stand = start_with(s).await;
    let port = stand.base.rsplit(':').next().unwrap().to_string();
    let named = reqwest::Client::new()
        .get(format!("{}/v1/models", stand.base))
        .header("host", format!("proxy.lan:{port}"))
        .bearer_auth("server-key")
        .send()
        .await
        .unwrap();
    assert_eq!(named.status(), 200);
    // Своё имя не отменяет ключ.
    let keyless = get_with_host(&stand, "/v1/models", &format!("proxy.lan:{port}")).await;
    assert_eq!(keyless.status(), 401);
    let foreign = reqwest::Client::new()
        .get(format!("{}/v1/models", stand.base))
        .header("host", "other.lan")
        .bearer_auth("server-key")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 403);
}

#[test]
fn a_blank_api_key_is_no_key_at_all() {
    for blank in ["", "   ", "\t"] {
        let mut config = crate::config::Config::default();
        config.server.api_key = Some(blank.to_string());
        assert_eq!(
            ServerSettings::from_config(&config).api_key,
            None,
            "{blank:?}"
        );
    }
}

#[tokio::test]
async fn the_trimmed_status_hides_subscription_accounts_and_routes() {
    set_keys();
    let up = upstream(vec![("/a/", vec![text_stream(&["x"])])]).await;
    let cli = FakeBackend::new(vec![Act::Reply("never")]);
    let gateway = engine(
        &config(up.port, true),
        vec![Arc::new(CliFactory(Arc::clone(&cli)))],
    );
    let open = start(gateway.clone(), None).await;
    let text = reqwest::get(format!("{}/v1/pooprusteek/status", open.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for hidden in ["sub", "oc", "cx", "cli:", "subscription", "opus"] {
        assert!(
            !text.contains(&format!("\"{hidden}")) && !text.contains(&format!("{hidden}\"")),
            "{hidden} in {text}"
        );
    }
    assert!(!text.contains("cli:"), "{text}");

    let keyed = start(gateway, Some("server-key")).await;
    let full: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/v1/pooprusteek/status", keyed.base))
        .bearer_auth("server-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = full["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["id"].as_str())
        .collect();
    assert!(ids.contains(&"sub") && ids.contains(&"a"), "{ids:?}");
}

// ── Уход клиента до первого байта ───────────────────────────────────────

/// Отправляет запрос сырым сокетом и закрывает его, не дождавшись ответа.
async fn send_and_hang_up(stand: &Stand, body: serde_json::Value) {
    use tokio::io::AsyncWriteExt;
    let addr = stand.base.trim_start_matches("http://").to_string();
    let mut socket = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let payload = body.to_string();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    drop(socket);
}

#[tokio::test]
async fn a_client_that_leaves_before_the_first_byte_cancels_the_upstream_at_once() {
    for stream in [true, false] {
        let (stand, up) = stand(vec![("/a/", vec![Script::Silent])]).await;
        send_and_hang_up(
            &stand,
            serde_json::json!({
                "model": "a/m", "stream": stream,
                "messages": [{"role": "user", "content": "hi"}],
            }),
        )
        .await;
        assert!(
            eventually(|| up.closed.load(std::sync::atomic::Ordering::SeqCst) == 1).await,
            "stream={stream}: the upstream kept generating for a client that was gone"
        );
        let ledger = stand.cell.get().ledger();
        assert!(
            eventually(|| ledger
                .totals(&LedgerQuery::default())
                .iter()
                .map(|r| r.calls)
                .sum::<u64>()
                == 1)
            .await,
            "stream={stream}: {:?}",
            ledger.totals(&LedgerQuery::default())
        );
        let rows = ledger.totals(&LedgerQuery::default());
        assert_eq!(rows[0].failed, 0, "a leaving client is not a failure");
    }
}

// ── Расход в потоке: клиента не задерживаем ─────────────────────────────

async fn stream_with_usage(
    stand: &Stand,
    model: &str,
    include_usage: bool,
) -> (String, std::time::Duration) {
    let started = std::time::Instant::now();
    let body = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", stand.base))
        .json(&serde_json::json!({
            "model": model,
            "stream": true,
            "stream_options": {"include_usage": include_usage},
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    (body, started.elapsed())
}

#[tokio::test]
async fn behind_the_gateway_a_lingering_upstream_delays_the_client_by_a_second_at_most() {
    let (stand, up) = stand(vec![("/a/", vec![Script::Linger(vec!["done"])])]).await;
    for include_usage in [true, false] {
        let (body, took) = stream_with_usage(&stand, "a/m", include_usage).await;
        assert!(body.contains("data: [DONE]"), "{body}");
        assert!(
            !body.contains("\"usage\""),
            "the upstream never sent any: {body}"
        );
        let limit = if include_usage { 1800 } else { 1000 };
        assert!(
            took.as_millis() < limit,
            "include_usage={include_usage}: the client waited {took:?}"
        );
    }
    // Клиент ушёл, а учёт остаётся ровно по одной записи на запрос.
    let ledger = stand.cell.get().ledger();
    assert!(
        eventually(|| ledger
            .totals(&LedgerQuery::default())
            .iter()
            .map(|r| r.calls)
            .sum::<u64>()
            == 2)
        .await,
        "{:?}",
        ledger.totals(&LedgerQuery::default())
    );
    drop(up);
}

#[tokio::test]
async fn with_the_gateway_off_include_usage_adds_no_waiting() {
    let up = upstream(vec![("/legacy/", vec![Script::Linger(vec!["done"])])]).await;
    let mut s = settings(
        Arc::new(crate::gateway::handle::DisabledGateway::new()),
        None,
        "127.0.0.1",
    );
    s.entries = vec![crate::config::ProviderEntry {
        name: "old".to_string(),
        base_url: format!("http://127.0.0.1:{}/legacy", up.port),
        api_key: None,
        model: "m".to_string(),
        protocol: crate::config::ProviderProtocol::Openai,
        tools: Default::default(),
    }];
    let stand = start_with(s).await;
    for include_usage in [true, false] {
        let (body, took) = stream_with_usage(&stand, "old/m", include_usage).await;
        assert!(body.contains("data: [DONE]"), "{body}");
        assert!(
            took.as_millis() < 1500,
            "include_usage={include_usage}: the client waited {took:?}"
        );
    }
}

// ── Потоковый `/v1/completions` и ответы с инструментами ────────────────

async fn legacy_post(stand: &Stand, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/completions", stand.base))
        .json(&serde_json::json!({"model": model, "prompt": "hi", "stream": true}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_streaming_legacy_completion_falls_back_and_reports_the_account() {
    let (stand, up) = stand(vec![
        ("/a/", vec![Script::RateLimited]),
        ("/b/", vec![text_stream(&["old ", "style"])]),
    ])
    .await;
    let response = legacy_post(&stand, "auto/main").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-pooprusteek-account").as_deref(),
        Some("b")
    );
    assert_eq!(
        header(&response, "x-pooprusteek-fallback").as_deref(),
        Some("a")
    );
    let body = response.text().await.unwrap();
    let text: String = frames(&body)
        .iter()
        .filter_map(|f| serde_json::from_str::<serde_json::Value>(f).ok())
        .filter_map(|v| v["choices"][0]["text"].as_str().map(str::to_string))
        .collect();
    assert_eq!(text, "old style");
    assert_eq!((up.hits("/a/"), up.hits("/b/")), (1, 1));
}

#[tokio::test]
async fn a_streaming_legacy_completion_fails_with_a_real_status() {
    let (stand, _up) = stand(vec![
        ("/a/", vec![Script::RateLimited]),
        ("/b/", vec![Script::RateLimited]),
    ])
    .await;
    let response = legacy_post(&stand, "auto/main").await;
    assert_eq!(response.status(), 429);
    assert!(header(&response, "retry-after").is_some());
}

fn tool_request(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "stream": true,
        "messages": [{"role": "user", "content": "write it"}],
        "tools": [{"type": "function", "function": {
            "name": "write",
            "description": "write a file",
            "parameters": {"type": "object", "properties": {}},
        }}],
    })
}

#[tokio::test]
async fn a_slow_tool_reply_keeps_the_connection_alive_and_a_fast_failure_keeps_its_status() {
    let call = "<tool_use><name>write</name><arguments>{}</arguments></tool_use>";
    let (stand, _up) = stand(vec![
        (
            "/a/",
            vec![Script::Slow(700, Box::new(text_stream(&[call])))],
        ),
        ("/b/", vec![Script::RateLimited]),
    ])
    .await;
    let mut response = post_body(&stand, tool_request("a/m")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-pooprusteek-account").as_deref(),
        Some("a")
    );
    let first = response
        .chunk()
        .await
        .unwrap()
        .expect("bytes before the reply is ready");
    assert!(
        String::from_utf8_lossy(&first).starts_with(": keep-alive"),
        "{:?}",
        String::from_utf8_lossy(&first)
    );
    let mut rest = String::new();
    while let Some(piece) = response.chunk().await.unwrap() {
        rest.push_str(&String::from_utf8_lossy(&piece));
    }
    assert!(rest.contains("tool_calls"), "{rest}");
    assert!(rest.contains("data: [DONE]"), "{rest}");

    // Отказ раньше порога остаётся настоящим кодом.
    let failed = post_body(&stand, tool_request("b/m")).await;
    assert_eq!(failed.status(), 429);
}
