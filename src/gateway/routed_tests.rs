//! Тесты `RoutedProvider`: fallback только до первого смыслового чанка, места на аккаунтах,
//! учёт, перенос истории. Адаптеры — сценарные, часы — ручные, настоящих пауз нет.

use super::backend::Capabilities;
use super::clock::{ManualClock, scratch_dir};
use super::handle::{
    AccountStatus, CallSource, ConfigEdit, Gateway, GatewayEvent, LedgerQuery, LimitSource,
};
use super::state::RouteState;
use super::testkit::{
    Act, FakeBackend, Rig, TWO_ACCOUNTS, ask, ask_with, caps, request, rig, rig_in,
};
use super::types::{AccountId, Role};
use crate::error::AppError;
use crate::provider::failure::FailureClass;
use crate::provider::{ChatMessage, ToolCall};
use std::sync::Arc;
use std::time::Duration;

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).unwrap()
}

fn rate_limit(secs: u64) -> FailureClass {
    FailureClass::RateLimit {
        retry_after: Some(Duration::from_secs(secs)),
    }
}

fn pair(a: Vec<Act>, b: Vec<Act>) -> Rig {
    rig(
        TWO_ACCOUNTS,
        vec![("a", FakeBackend::new(a)), ("b", FakeBackend::new(b))],
    )
}

fn failure_class(error: &AppError) -> Option<FailureClass> {
    error.failure_class()
}

fn status(rig: &Rig, account: &str) -> AccountStatus {
    rig.engine
        .snapshot()
        .accounts
        .into_iter()
        .find(|a| a.id.as_str() == account)
        .unwrap()
        .status
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

fn calls_in_history() -> Vec<ChatMessage> {
    let mut assistant = ChatMessage::assistant("looking");
    assistant.tool_calls = vec![ToolCall {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({"path": "a.rs"}),
        provider_state: Some(serde_json::json!({"thoughtSignature": "sig"})),
    }];
    vec![
        ChatMessage::user("read it"),
        assistant,
        ChatMessage::tool("call_1", "contents"),
        ChatMessage::user("and now?"),
    ]
}

#[tokio::test]
async fn a_rate_limited_first_account_falls_back_before_anything_is_shown() {
    let rig = pair(vec![Act::Fail(rate_limit(40))], vec![Act::Reply("from b")]);
    let mut events = rig.engine.subscribe();
    let provider = rig.main();

    let (result, text) = ask(&provider, "hello").await;
    result.expect("the turn completes on the second account");
    assert_eq!(text, "from b", "only b's words reached the receiver");
    assert_eq!(rig.fakes["a"].call_count(), 1);
    assert_eq!(rig.fakes["b"].call_count(), 1);

    let mut taken = None;
    while let Ok(event) = events.try_recv() {
        if let GatewayEvent::FallbackTaken {
            from, to, class, ..
        } = event
        {
            taken = Some((from, to, class));
        }
    }
    let (from, to, class) = taken.expect("the fallback was announced");
    assert_eq!((from.as_str(), to.as_str()), ("a", "b"));
    assert_eq!(class, rate_limit(40));
    assert!(matches!(status(&rig, "a"), AccountStatus::Cooling { .. }));
    assert_eq!(status(&rig, "b"), AccountStatus::Ready);

    // Обратная сторона: пока a остывает, второй вопрос к нему даже не идёт.
    let (again, text) = ask(&provider, "again").await;
    again.unwrap();
    assert_eq!(text, "ok");
    assert_eq!(rig.fakes["a"].call_count(), 1, "Retry-After is respected");
    assert_eq!(rig.fakes["b"].call_count(), 2);
}

/// Положительный случай для чистой беседы: после срока новый разговор снова идёт на a.
#[tokio::test]
async fn the_first_account_is_used_again_by_a_new_chat_once_its_wait_is_over() {
    let rig = pair(vec![Act::Fail(rate_limit(40))], vec![]);
    let chat = rig.main();
    ask(&chat, "one").await.0.unwrap();
    rig.advance(41);

    let (_, _) = ask(&chat, "two").await;
    assert_eq!(
        rig.fakes["a"].call_count(),
        1,
        "the chat stays on b: it is ready, and the cache lives there"
    );
    let fresh = rig.main();
    ask(&fresh, "three").await.0.unwrap();
    assert_eq!(
        rig.fakes["a"].call_count(),
        2,
        "a new chat starts from the top again"
    );
    assert_eq!(
        status(&rig, "a"),
        AccountStatus::Ready,
        "the success cleared it"
    );
}

#[tokio::test]
async fn once_text_has_gone_out_there_is_no_fallback_and_no_duplicate() {
    let rig = pair(
        vec![Act::FailAfterText("par", FailureClass::Server)],
        vec![Act::Reply("from b")],
    );
    let provider = rig.main();
    let (result, text) = ask(&provider, "hello").await;
    let error = result.expect_err("the stream broke after output");
    assert_eq!(
        text, "par",
        "exactly what was already shown, nothing glued on"
    );
    assert_eq!(failure_class(&error), Some(FailureClass::Server));
    assert_eq!(rig.fakes["b"].call_count(), 0, "b was never asked");
}

/// Критик A9: пустой живой сигнал — не вывод; fallback ещё разрешён.
#[tokio::test]
async fn a_heartbeat_before_the_failure_does_not_forbid_the_fallback() {
    let rig = pair(
        vec![Act::HeartbeatThenFail(FailureClass::Overloaded)],
        vec![Act::Reply("from b")],
    );
    let (result, text) = ask(&rig.main(), "hello").await;
    result.unwrap();
    assert_eq!(text, "from b");
}

#[tokio::test]
async fn a_bad_request_is_not_retried_elsewhere_and_does_not_punish_the_account() {
    let rig = pair(
        vec![Act::Fail(FailureClass::BadRequest)],
        vec![Act::Reply("b")],
    );
    let (result, text) = ask(&rig.main(), "hello").await;
    let error = result.expect_err("the request itself is wrong");
    assert_eq!(failure_class(&error), Some(FailureClass::BadRequest));
    assert_eq!(text, "");
    assert_eq!(rig.fakes["b"].call_count(), 0);
    assert_eq!(status(&rig, "a"), AccountStatus::Ready);
}

#[tokio::test]
async fn only_the_triggers_the_route_lists_move_the_turn_on() {
    let toml = TWO_ACCOUNTS.replace("id = \"main\"", "id = \"main\"\non = [\"rate_limit\"]");
    let rig = rig(
        &toml,
        vec![
            (
                "a",
                FakeBackend::new(vec![
                    Act::Fail(FailureClass::Auth),
                    Act::Fail(rate_limit(30)),
                ]),
            ),
            ("b", FakeBackend::new(vec![Act::Reply("from b")])),
        ],
    );
    let first = rig.main();
    let (auth, _) = ask(&first, "x").await;
    assert_eq!(failure_class(&auth.unwrap_err()), Some(FailureClass::Auth));
    assert_eq!(rig.fakes["b"].call_count(), 0, "auth is not on the list");
    assert!(
        matches!(status(&rig, "a"), AccountStatus::NeedsAttention { .. }),
        "but the account is still marked"
    );

    rig.advance(601);
    let second = rig.main();
    let (limited, text) = ask(&second, "y").await;
    limited.unwrap();
    assert_eq!(text, "from b", "rate limit is on the list");
}

#[tokio::test]
async fn when_everyone_fails_the_last_real_error_comes_back_with_its_class() {
    let rig = pair(
        vec![Act::Fail(FailureClass::Auth)],
        vec![Act::Fail(FailureClass::Overloaded)],
    );
    let (result, _) = ask(&rig.main(), "hello").await;
    let error = result.unwrap_err();
    assert_eq!(failure_class(&error), Some(FailureClass::Overloaded));
    assert!(error.to_string().contains("scripted failure"), "{error}");
}

#[tokio::test]
async fn with_everyone_cooling_a_fresh_call_fails_fast_naming_the_wait() {
    let rig = pair(
        vec![Act::Fail(rate_limit(40))],
        vec![Act::Fail(rate_limit(90))],
    );
    ask(&rig.main(), "first").await.0.unwrap_err();
    let other = rig.main();
    let (result, _) = ask(&other, "second").await;
    let error = result.unwrap_err();
    assert_eq!(
        failure_class(&error),
        Some(FailureClass::RateLimit {
            retry_after: Some(Duration::from_secs(40))
        }),
        "the nearest return is what a client should wait for: {error}"
    );
    assert_eq!(
        rig.fakes["a"].call_count() + rig.fakes["b"].call_count(),
        2,
        "no calls were wasted"
    );
}

// ── История при смене цели ──────────────────────────────────────────────

#[tokio::test]
async fn a_switch_turns_foreign_tool_calls_into_text_and_a_stay_leaves_them_alone() {
    let rig = pair(vec![], vec![]);
    let provider = rig.main();
    ask_with(&provider, calls_in_history()).await.0.unwrap();
    let kept = rig.fakes["a"].last_messages();
    assert_eq!(kept[1].tool_calls.len(), 1, "same target: untouched");
    assert!(kept[1].tool_calls[0].provider_state.is_some());

    // a остывает, беседа переезжает на b.
    let runtime = rig.engine.core().runtime(&id("a")).unwrap();
    rig.engine
        .core()
        .report_failure(&runtime, &id("a"), &rate_limit(300), "x");
    ask_with(&provider, calls_in_history()).await.0.unwrap();
    let moved = rig.fakes["b"].last_messages();
    assert!(moved[1].tool_calls.is_empty(), "no foreign ids reach b");
    assert!(
        moved[1]
            .content
            .contains("<tool_use><name>read_file</name>")
    );
    assert_eq!(moved[2].content, "contents", "results stay as they were");

    // Дальше на b: старая история всё ещё развёрнута, чужие подписи не возвращаются.
    ask_with(&provider, calls_in_history()).await.0.unwrap();
    assert!(rig.fakes["b"].last_messages()[1].tool_calls.is_empty());
}

#[tokio::test]
async fn a_restored_route_state_decides_whether_old_signatures_may_be_sent() {
    let rig = pair(vec![], vec![]);
    let strangers = rig.main();
    strangers.seed_route_state(RouteState {
        route: None,
        account: id("b"),
        model: "model-b".into(),
    });
    ask_with(&strangers, calls_in_history()).await.0.unwrap();
    assert!(
        rig.fakes["a"].last_messages()[1].tool_calls.is_empty(),
        "the history came from b, a must not see its ids"
    );

    let natives = rig.main();
    natives.seed_route_state(RouteState {
        route: None,
        account: id("a"),
        model: "model-a".into(),
    });
    ask_with(&natives, calls_in_history()).await.0.unwrap();
    assert_eq!(
        rig.fakes["a"].last_messages()[1].tool_calls.len(),
        1,
        "the same target gets its own history back"
    );
}

#[tokio::test]
async fn the_route_state_follows_the_last_successful_target() {
    let rig = pair(vec![Act::Fail(rate_limit(30))], vec![]);
    let provider = rig.main();
    assert!(provider.route_state().is_none(), "nothing yet");
    ask(&provider, "go").await.0.unwrap();
    let state = provider.route_state().expect("bound after a success");
    assert_eq!(
        (state.account.as_str(), state.model.as_str()),
        ("b", "model-b")
    );
    assert_eq!(state.route.unwrap().as_str(), "main");
}

fn server_session_caps() -> Capabilities {
    Capabilities {
        server_side_session: true,
        ..caps()
    }
}

#[tokio::test]
async fn leaving_a_server_session_discards_it_and_staying_keeps_it() {
    let a = FakeBackend::with_caps(server_session_caps(), vec![]);
    let rig = rig(
        TWO_ACCOUNTS,
        vec![("a", a), ("b", FakeBackend::new(vec![]))],
    );
    let provider = rig.main();
    ask(&provider, "one").await.0.unwrap();
    ask(&provider, "two").await.0.unwrap();
    settle().await;
    assert_eq!(
        rig.fakes["a"].discards(),
        0,
        "staying on a keeps its session"
    );
    assert_eq!(rig.fakes["a"].forks(), 1, "one session for the whole chat");

    let runtime = rig.engine.core().runtime(&id("a")).unwrap();
    rig.engine
        .core()
        .report_failure(&runtime, &id("a"), &rate_limit(300), "x");
    ask(&provider, "three").await.0.unwrap();
    settle().await;
    assert_eq!(
        rig.fakes["a"].discards(),
        1,
        "the old server session is deleted"
    );
    assert_eq!(rig.fakes["b"].discards(), 0);
}

// ── Форки, места, режим инструментов ────────────────────────────────────

#[tokio::test]
async fn a_fork_shares_the_engine_but_starts_with_no_attachment_to_an_account() {
    let rig = pair(vec![], vec![]);
    let parent = rig.main();
    ask(&parent, "hi").await.0.unwrap();
    let child = parent.fork();
    assert!(parent.route_state().is_some());
    assert!(child.route_state().is_none(), "a fork starts clean");
    assert_eq!(child.model(), parent.model());

    let runtime = rig.engine.core().runtime(&id("a")).unwrap();
    rig.engine
        .core()
        .report_failure(&runtime, &id("a"), &rate_limit(300), "x");
    ask(&child, "hi").await.0.unwrap();
    assert_eq!(
        child.route_state().unwrap().account.as_str(),
        "b",
        "the fork sees the same cooldown"
    );
    assert_eq!(
        parent.route_state().unwrap().account.as_str(),
        "a",
        "the parent is unmoved"
    );
}

const SINGLE_SLOT: &str = r#"
    [gateway]
    enabled = true
    [[accounts]]
    id = "a"
    provider = "custom"
    base_url = "http://a.invalid/v1"
    secret = "none"
    model = "model-a"
    [accounts.limits]
    max_concurrency = 1
    [[accounts]]
    id = "b"
    provider = "custom"
    base_url = "http://b.invalid/v1"
    secret = "none"
    model = "model-b"
    [[routes]]
    id = "main"
    steps = [{ account = "a" }, { account = "b" }]
    [roles]
    main = "main"
"#;

#[tokio::test]
async fn a_second_call_does_not_queue_behind_a_busy_account_while_another_is_free() {
    let rig = rig(
        SINGLE_SLOT,
        vec![
            ("a", FakeBackend::new(vec![Act::Hold])),
            ("b", FakeBackend::new(vec![Act::Reply("from b")])),
        ],
    );
    let first = rig.main();
    let held = tokio::spawn({
        let first = Arc::clone(&first);
        async move { ask(&first, "long one").await }
    });
    rig.fakes["a"].inner.started.notified().await;
    assert_eq!(rig.engine.snapshot().accounts[0].in_flight, 1);

    let (result, text) = ask(&rig.main(), "quick one").await;
    result.unwrap();
    assert_eq!(text, "from b", "a has one slot and it is taken");
    assert_eq!(rig.fakes["a"].call_count(), 1);

    rig.fakes["a"].inner.release.notify_one();
    let (done, text) = held.await.unwrap();
    done.unwrap();
    assert_eq!(text, "held");
    assert_eq!(
        rig.engine.snapshot().accounts[0].in_flight,
        0,
        "the slot was returned"
    );
}

#[tokio::test]
async fn with_a_single_account_the_second_call_waits_for_the_slot() {
    let only_a = SINGLE_SLOT.replace(
        "steps = [{ account = \"a\" }, { account = \"b\" }]",
        "steps = [{ account = \"a\" }]",
    );
    let rig = rig(
        &only_a,
        vec![
            ("a", FakeBackend::new(vec![Act::Hold, Act::Reply("second")])),
            ("b", FakeBackend::new(vec![])),
        ],
    );
    let first = rig.main();
    let held = tokio::spawn({
        let first = Arc::clone(&first);
        async move { ask(&first, "long one").await }
    });
    rig.fakes["a"].inner.started.notified().await;

    let second_provider = rig.main();
    let waiting = tokio::spawn(async move { ask(&second_provider, "next").await });
    settle().await;
    assert_eq!(
        rig.fakes["a"].call_count(),
        1,
        "the second call is queued, not running alongside"
    );

    rig.fakes["a"].inner.release.notify_one();
    held.await.unwrap().0.unwrap();
    let (result, text) = waiting.await.unwrap();
    result.unwrap();
    assert_eq!(text, "second");
    assert_eq!(rig.fakes["a"].call_count(), 2);
}

const NATIVE_TWO: &str = r#"
    [gateway]
    enabled = true
    [[accounts]]
    id = "a"
    provider = "custom"
    base_url = "http://a.invalid/v1"
    secret = "none"
    tools = "native"
    model = "model-a"
    [[accounts]]
    id = "b"
    provider = "custom"
    base_url = "http://b.invalid/v1"
    secret = "none"
    tools = "native"
    model = "model-b"
    [[routes]]
    id = "main"
    steps = [{ account = "a" }, { account = "b" }]
    [roles]
    main = "main"
"#;

#[tokio::test]
async fn the_tool_mode_is_a_constant_of_the_route_through_a_whole_turn() {
    let native = Capabilities {
        native_tools: true,
        ..caps()
    };
    let rig = rig(
        NATIVE_TWO,
        vec![
            (
                "a",
                FakeBackend::with_caps(native.clone(), vec![Act::Fail(rate_limit(30))]),
            ),
            ("b", FakeBackend::with_caps(native, vec![])),
        ],
    );
    let provider = rig.main();
    assert!(
        provider.native_tools(),
        "read once per turn, before any call"
    );
    ask(&provider, "go").await.0.unwrap();
    assert!(provider.native_tools(), "a switch to b does not change it");
    assert_eq!(
        rig.fakes["b"].inner.calls.lock().unwrap()[0].tool_mode,
        super::types::ToolMode::Native,
        "the adapter was told the mode of the turn"
    );

    let prompt = pair(vec![], vec![]);
    assert!(!prompt.main().native_tools());
}

// ── Учёт ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn usage_is_booked_once_per_attempt_and_a_failed_attempt_costs_no_tokens() {
    let rig = pair(
        vec![Act::Fail(FailureClass::Server)],
        vec![Act::ReplyWithUsage("answer", 100, 7)],
    );
    let provider = rig.main();
    ask(&provider, "go").await.0.unwrap();
    let rows = rig.engine.ledger().totals(&LedgerQuery::default());
    let a = rows.iter().find(|r| r.account.as_str() == "a").unwrap();
    let b = rows.iter().find(|r| r.account.as_str() == "b").unwrap();
    assert_eq!((a.calls, a.failed, a.prompt_tokens), (1, 1, 0));
    assert_eq!(
        (b.calls, b.fallbacks, b.prompt_tokens, b.completion_tokens),
        (1, 1, 100, 7),
        "the usage chunk after the finish was counted, and only once"
    );
    assert_eq!(b.source, CallSource::Chat);
    assert!(!b.any_estimated);

    ask(&provider, "again").await.0.unwrap();
    let again = rig.engine.ledger().totals(&LedgerQuery {
        account: Some(id("b")),
        since: None,
    });
    assert_eq!(again[0].calls, 2);
}

#[tokio::test]
async fn the_ledger_reaches_the_disk_on_a_flush_and_a_restart_reads_it_back() {
    let dir = scratch_dir("ledger-engine");
    let first = rig_in(
        TWO_ACCOUNTS,
        vec![
            ("a", FakeBackend::new(vec![Act::ReplyWithUsage("x", 50, 5)])),
            ("b", FakeBackend::new(vec![])),
        ],
        Some(dir.clone()),
        ManualClock::new(1_000_000),
    );
    ask(&first.main(), "go").await.0.unwrap();
    first.engine.core().flush();
    drop(first);
    let second = rig_in(
        TWO_ACCOUNTS,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
        Some(dir),
        ManualClock::new(1_000_100),
    );
    let rows = second.engine.ledger().totals(&LedgerQuery::default());
    assert_eq!((rows[0].account.as_str(), rows[0].prompt_tokens), ("a", 50));
}

#[tokio::test]
async fn the_decision_log_keeps_recent_choices_with_their_reasons() {
    let rig = pair(vec![Act::Fail(rate_limit(40))], vec![]);
    ask(&rig.main(), "go").await.0.unwrap();
    let recent = rig.engine.snapshot().recent;
    assert_eq!(
        recent.len(),
        2,
        "the first choice and the one after the failure"
    );
    assert!(recent[0].explain().starts_with("a · model-a"));
    assert!(
        recent[1]
            .explain()
            .contains("b · model-b: a failed (rate_limit)"),
        "{}",
        recent[1].explain()
    );
}

#[tokio::test]
async fn a_declared_rate_is_shown_as_an_estimate_and_never_blocks_the_account() {
    let toml = TWO_ACCOUNTS.replace(
        "model = \"model-a\"",
        "model = \"model-a\"\n[accounts.limits]\nrequests_per_minute = 2",
    );
    let rig = rig(
        &toml,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
    );
    let provider = rig.main();
    for _ in 0..4 {
        ask(&provider, "x").await.0.unwrap();
    }
    let view = rig.engine.snapshot().accounts.remove(0);
    let estimate = view.limits.expect("an estimate is shown");
    assert_eq!(
        estimate.source,
        LimitSource::Ledger,
        "marked as ours, drawn with ~"
    );
    assert_eq!(estimate.max_used_pct(), Some(100.0));
    assert_eq!(
        rig.fakes["a"].call_count(),
        4,
        "four calls against a declared two a minute: still all on a"
    );
}

// ── Возврат без человека ────────────────────────────────────────────────

#[tokio::test]
async fn an_account_marked_for_attention_is_tried_again_by_itself_and_recovers() {
    let rig = pair(vec![Act::Fail(FailureClass::Auth)], vec![]);
    ask(&rig.main(), "x").await.0.unwrap();
    assert!(matches!(
        status(&rig, "a"),
        AccountStatus::NeedsAttention { .. }
    ));

    rig.advance(599);
    ask(&rig.main(), "y").await.0.unwrap();
    assert_eq!(rig.fakes["a"].call_count(), 1, "not yet");

    rig.advance(2);
    ask(&rig.main(), "z").await.0.unwrap();
    assert_eq!(rig.fakes["a"].call_count(), 2, "the probe went to a");
    assert_eq!(
        status(&rig, "a"),
        AccountStatus::Ready,
        "and its success cleared the mark"
    );
}

#[tokio::test]
async fn after_a_restart_a_stale_hint_does_not_keep_the_only_account_idle() {
    let dir = scratch_dir("restart-hint");
    let before = rig_in(
        TWO_ACCOUNTS,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
        Some(dir.clone()),
        ManualClock::new(1_000_000),
    );
    let runtime = before.engine.core().runtime(&id("a")).unwrap();
    before.engine.core().report_failure(
        &runtime,
        &id("a"),
        &FailureClass::QuotaExhausted { resets_at: None },
        "x",
    );
    before.engine.core().flush();
    drop(before);

    let only_a = TWO_ACCOUNTS.replace(
        "steps = [{ account = \"a\" }, { account = \"b\" }]",
        "steps = [{ account = \"a\" }]",
    );
    let after = rig_in(
        &only_a,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
        Some(dir.clone()),
        ManualClock::new(1_000_100),
    );
    let (result, text) = ask(&after.main(), "x").await;
    result.expect("the hint is only a hint: the first call decides");
    assert_eq!(text, "ok");
    after.engine.core().flush();
    let saved = std::fs::read_to_string(dir.join("state.json")).unwrap();
    assert!(
        !saved.contains("\"account\": \"a\""),
        "success cleared the hint: {saved}"
    );
}

#[tokio::test]
async fn complete_collects_the_streamed_reply_and_its_usage() {
    let rig = pair(vec![Act::ReplyWithUsage("whole", 9, 3)], vec![]);
    let provider = rig.main();
    let response = provider
        .complete(request(vec![ChatMessage::user("q")]))
        .await
        .unwrap();
    assert_eq!(response.content, "whole");
    assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    assert_eq!(response.usage.unwrap().prompt_tokens, 9);
}

#[tokio::test]
async fn listing_models_names_them_by_account() {
    let rig = pair(vec![], vec![]);
    let models = rig.main().list_models().await.unwrap();
    assert_eq!(models, ["a/listed", "b/listed"]);
}

#[tokio::test]
async fn editing_the_route_while_a_chat_is_open_takes_effect_on_its_next_call() {
    let rig = pair(vec![], vec![]);
    let provider = rig.main();
    ask(&provider, "one").await.0.unwrap();
    rig.engine
        .apply_edit(ConfigEdit::SetEnabled {
            id: id("a"),
            enabled: false,
        })
        .unwrap();
    let (result, _) = ask(&provider, "two").await;
    result.unwrap();
    assert_eq!(
        rig.fakes["a"].call_count(),
        1,
        "a was switched off mid-chat"
    );
    assert_eq!(rig.fakes["b"].call_count(), 1);

    rig.engine
        .apply_edit(ConfigEdit::SetRole {
            role: Role::Main,
            route: None,
        })
        .unwrap();
    let (gone, _) = ask(&provider, "three").await;
    assert!(gone.unwrap_err().to_string().contains("no longer exists"));
}
