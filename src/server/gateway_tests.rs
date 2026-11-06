//! `/serve` через шлюз: настоящий локальный сервер с одной стороны, настоящий «апстрим» на TCP с другой.
//! Проверяется то, что видит клиент: коды, поток, заголовки, расход, каталог, статус.

use super::testkit::{
    Script, Stand, Upstream, engine, eventually, settings, start, start_with, upstream,
};
use crate::gateway::handle::{AccountStatus, LedgerQuery};
use crate::gateway::testkit::{Act, FakeBackend};
use std::sync::Arc;

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
    struct Factory(Arc<FakeBackend>);
    impl crate::gateway::engine::BackendFactory for Factory {
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
    let gateway = engine(
        &config(up.port, true),
        vec![Arc::new(Factory(Arc::clone(&cli)))],
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
    let loopback = start(gateway.clone(), None).await;
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

    // Открытый на сеть сервер без ключа статус не отдаёт вовсе.
    let open = start_with(settings(gateway, None, "0.0.0.0")).await;
    let refused = reqwest::get(format!("{}/v1/pooprusteek/status", open.base))
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
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
