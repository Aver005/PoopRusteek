//! Тесты выбора цели: каждое решение проверено и «взял», и «не взял», с причиной.
//! Часы ручные, настоящих `sleep` нет.

use super::backend::Capabilities;
use super::clock::{Clock, ManualClock, SystemClock, scratch_dir};
use super::engine::{
    BackendFactory, Binding, BuiltinFactory, Engine, EngineDeps, EnvOnlyResolver, Secret,
    SecretError, SecretResolver, build_gateway, gateway_config,
};
use super::handle::{
    AccountStatus, CallSource, ConfigEdit, Decision, DecisionQuery, EditError, Gateway,
    GatewayEvent, LimitSnapshot, LimitSource, LimitWindow, ResolveError, SkipReason, Target, Why,
    WindowKind, main_provider,
};
use super::testkit::{FakeBackend, Rig, TWO_ACCOUNTS, caps, parse, rig, rig_in};
use super::types::{
    Account, AccountId, AccountProvider, GatewayConfig, Role, SecretRef, Severity, ToolMode,
};
use crate::config::{Config, ToolProtocol};
use crate::provider::failure::FailureClass;
use std::sync::Arc;
use std::time::Duration;

fn id(raw: &str) -> AccountId {
    AccountId::parse(raw).unwrap()
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

fn two() -> Rig {
    rig(
        TWO_ACCOUNTS,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
    )
}

fn fail(rig: &Rig, account: &str, class: FailureClass) {
    let account = id(account);
    let runtime = rig.engine.core().runtime(&account).unwrap();
    rig.engine
        .core()
        .report_failure(&runtime, &account, &class, "scripted");
}

fn rate_limit(secs: u64) -> FailureClass {
    FailureClass::RateLimit {
        retry_after: Some(Duration::from_secs(secs)),
    }
}

/// Снимок лимитов на `pct` процентов, наблюдённый `age_secs` назад, с заданного источника.
fn set_limits(rig: &Rig, account: &str, pct: f32, age_secs: u64, source: LimitSource) {
    let account = id(account);
    let runtime = rig.engine.core().runtime(&account).unwrap();
    let snapshot = LimitSnapshot {
        account,
        plan: None,
        windows: vec![LimitWindow {
            name: "requests".into(),
            kind: WindowKind::Requests,
            used_pct: Some(pct),
            limit: None,
            remaining: None,
            resets_at: None,
        }],
        observed_at: rig.clock.now() - Duration::from_secs(age_secs),
        source,
    };
    runtime.state.lock().unwrap().limits = Some(snapshot);
}

fn target(decision: &Decision) -> Option<&str> {
    decision.target.as_ref().map(|t| t.account.as_str())
}

fn skip_of<'d>(decision: &'d Decision, account: &str) -> Option<&'d SkipReason> {
    decision
        .skipped
        .iter()
        .find(|s| s.account.as_str() == account)
        .map(|s| &s.reason)
}

#[test]
fn the_first_ready_step_wins_and_nothing_is_skipped() {
    let rig = two();
    let decision = rig.engine.decide(&query());
    assert_eq!(target(&decision), Some("a"));
    assert_eq!(decision.target.as_ref().unwrap().model, "model-a");
    assert_eq!(decision.why, Why::FirstReady);
    assert!(decision.skipped.is_empty());
    assert_eq!(decision.route.as_ref().map(|r| r.as_str()), Some("main"));
    assert!(decision.explain().contains("a · model-a: first ready step"));
}

/// Положительный и отрицательный случай разом: Retry-After держит аккаунт ровно до срока.
#[test]
fn a_cooling_account_is_skipped_with_its_wait_and_returns_when_the_clock_says() {
    let rig = two();
    fail(&rig, "a", rate_limit(40));
    let until = rig.clock.now() + Duration::from_secs(40);

    let during = rig.engine.decide(&query());
    assert_eq!(target(&during), Some("b"));
    assert_eq!(during.why, Why::OnlyCandidate);
    assert_eq!(
        skip_of(&during, "a"),
        Some(&SkipReason::Cooling { until }),
        "{}",
        during.explain()
    );
    assert!(during.explain().contains("a: cooling down"));

    rig.advance(39);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("b"),
        "one second early"
    );
    rig.advance(1);
    let after = rig.engine.decide(&query());
    assert_eq!(target(&after), Some("a"), "the deadline frees it");
    assert!(after.skipped.is_empty());
}

/// LiteLLM #34399: при нескольких аккаунтах `Retry-After` каждого соблюдается отдельно.
#[test]
fn retry_after_is_kept_per_account_when_several_are_cooling() {
    let rig = two();
    fail(&rig, "a", rate_limit(40));
    fail(&rig, "b", rate_limit(5));
    let nobody = rig.engine.decide(&query());
    assert_eq!(target(&nobody), None);
    assert_eq!(
        nobody.retry_at,
        Some(rig.clock.now() + Duration::from_secs(5)),
        "the nearest return is named"
    );
    rig.advance(10);
    assert_eq!(target(&rig.engine.decide(&query())), Some("b"));
    let still = rig.engine.decide(&query());
    assert!(
        matches!(skip_of(&still, "a"), Some(SkipReason::Cooling { .. })),
        "ten seconds into a forty-second wait a is still out"
    );
    rig.advance(31);
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"));
}

#[test]
fn a_failure_cools_only_the_account_that_failed() {
    let rig = two();
    fail(&rig, "a", rate_limit(40));
    let snapshot = rig.engine.snapshot();
    let status = |name: &str| {
        snapshot
            .accounts
            .iter()
            .find(|a| a.id.as_str() == name)
            .unwrap()
            .status
            .clone()
    };
    assert!(matches!(status("a"), AccountStatus::Cooling { .. }));
    assert_eq!(
        status("b"),
        AccountStatus::Ready,
        "the neighbour is untouched"
    );
}

#[test]
fn nobody_left_means_no_target_the_reason_and_the_nearest_return() {
    let rig = two();
    fail(&rig, "a", rate_limit(40));
    fail(&rig, "b", rate_limit(20));
    let decision = rig.engine.decide(&query());
    assert_eq!(decision.target, None);
    assert_eq!(decision.why, Why::RouteExhausted);
    assert_eq!(
        decision.retry_at,
        Some(rig.clock.now() + Duration::from_secs(20))
    );
    assert!(decision.explain().starts_with("no account available"));
    assert!(decision.explain().contains("a: cooling down"));
}

#[test]
fn switching_an_account_off_removes_it_and_switching_it_on_brings_it_back() {
    let rig = two();
    rig.engine
        .apply_edit(ConfigEdit::SetEnabled {
            id: id("a"),
            enabled: false,
        })
        .unwrap();
    let off = rig.engine.decide(&query());
    assert_eq!(target(&off), Some("b"));
    assert_eq!(skip_of(&off, "a"), Some(&SkipReason::Disabled));
    rig.engine
        .apply_edit(ConfigEdit::SetEnabled {
            id: id("a"),
            enabled: true,
        })
        .unwrap();
    let on = rig.engine.decide(&query());
    assert_eq!(target(&on), Some("a"));
    assert!(on.skipped.is_empty());
}

#[test]
fn a_native_turn_skips_prompt_only_accounts_but_a_prompt_turn_takes_them() {
    let rig = two();
    let native = rig.engine.decide(&DecisionQuery {
        tool_mode: ToolMode::Native,
        ..query()
    });
    assert_eq!(target(&native), None);
    assert_eq!(skip_of(&native, "a"), Some(&SkipReason::ToolModeMismatch));
    assert_eq!(skip_of(&native, "b"), Some(&SkipReason::ToolModeMismatch));
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"));
}

#[test]
fn a_cli_that_is_not_a_plain_model_is_never_picked_for_a_tool_turn() {
    let impure = FakeBackend::with_caps(
        Capabilities {
            pure_model: false,
            ..caps()
        },
        vec![],
    );
    let rig = rig(
        TWO_ACCOUNTS,
        vec![("a", impure), ("b", FakeBackend::new(vec![]))],
    );
    let decision = rig.engine.decide(&query());
    assert_eq!(target(&decision), Some("b"));
    assert_eq!(skip_of(&decision, "a"), Some(&SkipReason::NotPureModel));
}

#[test]
fn a_small_context_window_skips_the_account_only_when_the_input_does_not_fit() {
    let rig = two();
    rig.engine.core().learn_window(&id("a"), "model-a", 8_192);
    let big = rig.engine.decide(&DecisionQuery {
        input_tokens: Some(50_000),
        ..query()
    });
    assert_eq!(target(&big), Some("b"));
    assert_eq!(
        skip_of(&big, "a"),
        Some(&SkipReason::ContextTooSmall {
            need: 50_000,
            have: 8_192
        })
    );
    let small = rig.engine.decide(&DecisionQuery {
        input_tokens: Some(4_000),
        ..query()
    });
    assert_eq!(target(&small), Some("a"), "it fits, so it stays first");
    // Окно b неизвестно, и это не повод для отказа: неизвестное не запрещает.
    rig.engine.core().learn_window(&id("a"), "model-a", 100);
    let unknown = rig.engine.decide(&DecisionQuery {
        input_tokens: Some(900_000),
        ..query()
    });
    assert_eq!(target(&unknown), Some("b"));
}

fn with_limits() -> Rig {
    rig(
        &TWO_ACCOUNTS.replace(
            "model = \"model-a\"",
            "model = \"model-a\"\n[accounts.limits]\nhard_pct = 97\nsoft_pct = 85",
        ),
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
    )
}

/// Порог «заранее» — только по свежему снимку от сервера; оценка и старое число не решают.
#[test]
fn only_a_fresh_server_reading_can_remove_an_account_at_its_hard_limit() {
    let rig = with_limits();
    set_limits(&rig, "a", 99.0, 10, LimitSource::Header);
    let fresh = rig.engine.decide(&query());
    assert_eq!(target(&fresh), Some("b"));
    assert!(matches!(
        skip_of(&fresh, "a"),
        Some(SkipReason::OverHardLimit { .. })
    ));
    assert_eq!(fresh.snapshot_age, Some(Duration::from_secs(10)));

    set_limits(&rig, "a", 99.0, 600, LimitSource::Header);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "a reading ten minutes old decides nothing"
    );
    set_limits(&rig, "a", 99.0, 5, LimitSource::Ledger);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "an estimate from our own count never blocks"
    );
    set_limits(&rig, "a", 99.0, 5, LimitSource::Declared);
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"));
    set_limits(&rig, "a", 99.0, 5, LimitSource::CliEvent);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("b"),
        "the CLI's own event counts"
    );
}

#[test]
fn the_soft_threshold_moves_an_account_down_the_list_but_never_out() {
    let rig = with_limits();
    set_limits(&rig, "a", 90.0, 5, LimitSource::Header);
    let over = rig.engine.decide(&query());
    assert_eq!(target(&over), Some("b"));
    assert!(matches!(
        skip_of(&over, "a"),
        Some(SkipReason::OverSoftLimit { .. })
    ));
    assert!(
        over.explain().contains("a: at 90% (reading 5s old)"),
        "{}",
        over.explain()
    );

    set_limits(&rig, "a", 80.0, 5, LimitSource::Header);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "below the line"
    );
    set_limits(&rig, "a", 90.0, 500, LimitSource::Header);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "stale reading"
    );
    set_limits(&rig, "a", 90.0, 5, LimitSource::Ledger);
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"), "estimate");

    // Больше брать некого: аккаунт выше мягкого порога всё равно работает.
    set_limits(&rig, "a", 90.0, 5, LimitSource::Header);
    fail(&rig, "b", rate_limit(60));
    let alone = rig.engine.decide(&query());
    assert_eq!(target(&alone), Some("a"));
    assert_eq!(alone.why, Why::OnlyCandidate);
}

#[test]
fn a_route_threshold_works_like_the_account_one() {
    let toml = TWO_ACCOUNTS.replace("id = \"main\"", "id = \"main\"\nswitch_at_pct = 90");
    let rig = rig(
        &toml,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
    );
    set_limits(&rig, "a", 92.0, 5, LimitSource::Header);
    assert_eq!(target(&rig.engine.decide(&query())), Some("b"));
    set_limits(&rig, "a", 70.0, 5, LimitSource::Header);
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"));
}

#[test]
fn a_chat_stays_on_its_account_while_it_is_ready_and_leaves_when_it_is_not() {
    let rig = two();
    let stay = rig.engine.decide(&DecisionQuery {
        sticky: Some(id("b")),
        ..query()
    });
    assert_eq!(target(&stay), Some("b"));
    assert_eq!(stay.why, Why::Sticky);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "no habit, no stickiness"
    );

    fail(&rig, "b", rate_limit(30));
    let leave = rig.engine.decide(&DecisionQuery {
        sticky: Some(id("b")),
        ..query()
    });
    assert_eq!(target(&leave), Some("a"));
    assert_ne!(leave.why, Why::Sticky);
}

#[test]
fn a_busy_account_is_passed_over_and_when_all_are_busy_the_first_is_queued_for() {
    let rig = two();
    let core = rig.engine.core();
    let a = core.runtime(&id("a")).unwrap();
    let b = core.runtime(&id("b")).unwrap();
    let held_a: Vec<_> = (0..4)
        .map(|_| Arc::clone(&a.sem).try_acquire_owned().unwrap())
        .collect();

    let busy = rig.engine.decide(&query());
    assert_eq!(target(&busy), Some("b"));
    assert_eq!(skip_of(&busy, "a"), Some(&SkipReason::Busy));
    assert_eq!(rig.engine.snapshot().accounts[0].in_flight, 4);

    let held_b: Vec<_> = (0..4)
        .map(|_| Arc::clone(&b.sem).try_acquire_owned().unwrap())
        .collect();
    let plan = core.plan(&Binding::Role(Role::Main)).unwrap();
    let all = core.select(&plan, &query(), None);
    assert!(all.queue, "nobody is free: wait for the first");
    assert_eq!(target(&all.decision), Some("a"));
    assert!(all.decision.skipped.is_empty(), "queued, not skipped");

    drop(held_a);
    drop(held_b);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "a slot opened up"
    );
}

const WITH_CLAUDE_CLI: &str = r#"
    [gateway]
    enabled = true
    [[accounts]]
    id = "c"
    provider = "cli:claude"
    home = "/home/c"
    model = "opus"
    [[accounts]]
    id = "b"
    provider = "custom"
    base_url = "http://b.invalid/v1"
    secret = "none"
    model = "model-b"
    [[routes]]
    id = "main"
    steps = [{ account = "c" }, { account = "b" }]
    [roles]
    main = "main"
"#;

#[test]
fn the_claude_subscription_is_never_served_to_outside_clients() {
    let rig = rig(
        WITH_CLAUDE_CLI,
        vec![
            ("c", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
    );
    let chat = rig.engine.decide(&query());
    assert_eq!(
        target(&chat),
        Some("c"),
        "inside the app the CLI is a normal step"
    );
    let serve = rig.engine.decide(&DecisionQuery {
        source: CallSource::Serve,
        ..query()
    });
    assert_eq!(target(&serve), Some("b"));
    assert_eq!(skip_of(&serve, "c"), Some(&SkipReason::NotForServe));

    assert!(
        rig.engine
            .provider_for_model_id("c/opus", CallSource::Chat)
            .is_ok()
    );
    assert_eq!(
        rig.engine
            .provider_for_model_id("c/opus", CallSource::Serve)
            .err(),
        Some(ResolveError::Unknown("c/opus".into()))
    );
    assert!(
        rig.engine
            .provider_for_model_id("b/x", CallSource::Serve)
            .is_ok()
    );
}

#[test]
fn a_pinned_account_is_used_as_is_and_the_rest_say_why_they_were_not() {
    let rig = two();
    let pin = Target {
        account: id("b"),
        model: "model-b".into(),
    };
    let pinned = rig.engine.decide(&DecisionQuery {
        pinned: Some(pin.clone()),
        ..query()
    });
    assert_eq!(target(&pinned), Some("b"));
    assert_eq!(pinned.why, Why::Pinned);
    assert_eq!(skip_of(&pinned, "a"), Some(&SkipReason::PinnedElsewhere));

    // Закреплённый аккаунт в cooldown не подменяется соседом: человек просил его.
    fail(&rig, "b", rate_limit(30));
    let cooling = rig.engine.decide(&DecisionQuery {
        pinned: Some(pin),
        ..query()
    });
    assert_eq!(target(&cooling), None);
    assert!(matches!(
        skip_of(&cooling, "b"),
        Some(SkipReason::Cooling { .. })
    ));
    assert!(cooling.retry_at.is_some());
}

/// new-api #3537: аккаунт, отложенный системой, возвращается на пробу сам.
#[test]
fn an_account_set_aside_for_auth_comes_back_by_itself_after_a_while() {
    let rig = two();
    fail(&rig, "a", FailureClass::Auth);
    let away = rig.engine.decide(&query());
    assert_eq!(skip_of(&away, "a"), Some(&SkipReason::NeedsAttention));
    rig.advance(599);
    assert_eq!(target(&rig.engine.decide(&query())), Some("b"));
    rig.advance(1);
    assert_eq!(
        target(&rig.engine.decide(&query())),
        Some("a"),
        "the recheck window opens"
    );
}

// ── После перезапуска ───────────────────────────────────────────────────

const ONLY_A: &str = r#"
    [gateway]
    enabled = true
    [[accounts]]
    id = "a"
    provider = "custom"
    base_url = "http://a.invalid/v1"
    secret = "none"
    model = "model-a"
    [[routes]]
    id = "main"
    steps = [{ account = "a" }]
    [roles]
    main = "main"
"#;

fn restarted(toml: &str, dir: &std::path::Path, start: u64) -> Rig {
    rig_in(
        toml,
        vec![
            ("a", FakeBackend::new(vec![])),
            ("b", FakeBackend::new(vec![])),
        ],
        Some(dir.to_path_buf()),
        ManualClock::new(start),
    )
}

/// CLIProxyAPI #5404: записанное «исчерпан» после перезапуска — подсказка, а не запрет.
#[test]
fn a_saved_exhausted_hint_only_moves_the_account_to_the_back_and_never_blocks_it() {
    let dir = scratch_dir("hint");
    let before = restarted(TWO_ACCOUNTS, &dir, 1_000_000);
    fail(
        &before,
        "a",
        FailureClass::QuotaExhausted { resets_at: None },
    );
    before.engine.core().flush();
    assert!(dir.join("state.json").exists(), "the hint was written");
    drop(before);

    let after = restarted(TWO_ACCOUNTS, &dir, 1_000_100);
    let ordered = after.engine.decide(&query());
    assert_eq!(target(&ordered), Some("b"), "the hinted account goes last");
    assert!(matches!(
        skip_of(&ordered, "a"),
        Some(SkipReason::Exhausted { resets_at: Some(_) })
    ));
    assert_eq!(
        after.engine.snapshot().accounts[0].status,
        AccountStatus::Ready,
        "but it is not blocked"
    );

    let alone = restarted(ONLY_A, &dir, 1_000_100);
    assert_eq!(
        target(&alone.engine.decide(&query())),
        Some("a"),
        "with nobody else the live account is tried anyway"
    );
    drop((after, alone));

    let much_later = restarted(TWO_ACCOUNTS, &dir, 1_000_000 + 3_600);
    let fresh = much_later.engine.decide(&query());
    assert_eq!(target(&fresh), Some("a"), "an expired hint is ignored");
    assert!(fresh.skipped.is_empty());
}

#[test]
fn a_garbage_state_file_cannot_block_anything() {
    let dir = scratch_dir("hint-bad");
    std::fs::write(dir.join("state.json"), "{{{").unwrap();
    let rig = restarted(TWO_ACCOUNTS, &dir, 1_000_000);
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"));
}

// ── Правки, снимок, адреса ──────────────────────────────────────────────

#[test]
fn an_invalid_edit_is_refused_and_changes_nothing() {
    let rig = two();
    let mut rx = rig.engine.subscribe();
    let mut ghost = rig.engine.snapshot().routes[0].clone();
    ghost.steps.push(crate::gateway::types::Step {
        account: id("ghost"),
        model: None,
    });
    let result = rig
        .engine
        .apply_edit(ConfigEdit::UpsertRoute(Box::new(ghost)));
    assert!(
        matches!(&result, Err(EditError::Invalid(items)) if items.iter().any(|i| i.contains("unknown account 'ghost'"))),
        "the route would point at nobody: {result:?}"
    );
    assert_eq!(rig.engine.snapshot().routes[0].steps.len(), 2);
    assert!(rx.try_recv().is_err(), "no event for a refused edit");
    assert_eq!(
        rig.engine
            .apply_edit(ConfigEdit::SetEnabled {
                id: id("ghost"),
                enabled: true
            })
            .err(),
        Some(EditError::NotFound("ghost".into()))
    );
}

/// Удаление аккаунта, стоящего шагом маршрута: шаги уходят той же правкой, конфиг остаётся годным.
#[test]
fn removing_an_account_takes_its_steps_out_of_every_route_in_one_edit() {
    let rig = two();
    let mut rx = rig.engine.subscribe();
    rig.engine
        .apply_edit(ConfigEdit::RemoveAccount(id("a")))
        .expect("a is a step of main, and that is not a reason to refuse");
    let snapshot = rig.engine.snapshot();
    let ids: Vec<_> = snapshot.accounts.iter().map(|a| a.id.to_string()).collect();
    assert_eq!(ids, ["b"]);
    let steps: Vec<_> = snapshot.routes[0]
        .steps
        .iter()
        .map(|s| s.account.to_string())
        .collect();
    assert_eq!(steps, ["b"], "the step of the removed account is gone");
    assert!(
        snapshot
            .problems
            .iter()
            .all(|p| p.severity != crate::gateway::types::Severity::Error),
        "{:?}",
        snapshot.problems
    );
    assert!(
        matches!(rx.try_recv(), Ok(GatewayEvent::ConfigChanged)),
        "one edit, one event"
    );
    assert!(rx.try_recv().is_err());
}

/// Обратная сторона: единственный шаг не снимаем молча, ответ называет маршрут и роль.
#[test]
fn removing_the_only_step_of_a_route_is_refused_with_the_route_and_the_role() {
    let rig = two();
    rig.engine
        .apply_edit(ConfigEdit::RemoveAccount(id("a")))
        .unwrap();
    let mut rx = rig.engine.subscribe();
    let result = rig.engine.apply_edit(ConfigEdit::RemoveAccount(id("b")));
    let Err(EditError::Invalid(items)) = &result else {
        panic!("must be refused: {result:?}");
    };
    assert!(
        items[0].contains("route main")
            && items[0].contains("role main")
            && items[0].contains("no steps"),
        "{items:?}"
    );
    let snapshot = rig.engine.snapshot();
    assert_eq!(snapshot.accounts.len(), 1, "nothing changed");
    assert_eq!(snapshot.routes[0].steps.len(), 1);
    assert!(rx.try_recv().is_err(), "no event for a refused edit");
}

#[test]
fn a_valid_edit_announces_itself_and_keeps_the_neighbours_cooldown() {
    let rig = two();
    fail(&rig, "a", rate_limit(300));
    let mut rx = rig.engine.subscribe();
    rig.engine
        .apply_edit(ConfigEdit::SetLabel {
            id: id("b"),
            label: "Backup".into(),
        })
        .unwrap();
    assert!(matches!(rx.try_recv(), Ok(GatewayEvent::ConfigChanged)));
    let snapshot = rig.engine.snapshot();
    assert_eq!(snapshot.accounts[1].label, "Backup");
    assert_eq!(
        snapshot.accounts[1].id.as_str(),
        "b",
        "the id never changes"
    );
    assert!(
        matches!(snapshot.accounts[0].status, AccountStatus::Cooling { .. }),
        "renaming b must not reset a"
    );
    rig.engine
        .apply_edit(ConfigEdit::MoveAccount {
            id: id("b"),
            to_index: 0,
        })
        .unwrap();
    assert_eq!(rig.engine.snapshot().accounts[0].id.as_str(), "b");
}

#[test]
fn model_addresses_resolve_to_a_fixed_account_a_route_or_nothing() {
    let rig = two();
    let resolve = |id: &str| rig.engine.provider_for_model_id(id, CallSource::Chat);
    assert_eq!(resolve("a/custom-model").unwrap().model(), "a/custom-model");
    assert_eq!(resolve("auto/main").unwrap().model(), "auto/main");
    assert_eq!(
        resolve("b").unwrap().model(),
        "b/model-b",
        "a bare account name"
    );
    assert_eq!(
        resolve("model-b").unwrap().model(),
        "b/model-b",
        "a bare model name"
    );
    for bad in ["nope", "ghost/x", "auto/ghost", "a/"] {
        assert_eq!(
            resolve(bad).err(),
            Some(ResolveError::Unknown(bad.to_string())),
            "{bad}"
        );
    }
    rig.engine
        .apply_edit(ConfigEdit::SetEnabledGateway(false))
        .unwrap();
    assert!(!rig.engine.is_enabled());
    assert!(
        rig.engine
            .provider_for_role(Role::Main, CallSource::Chat)
            .is_none()
    );
    assert_eq!(resolve("a/x").err(), Some(ResolveError::Disabled));
}

#[test]
fn a_role_without_a_route_gets_no_provider_so_the_caller_stays_on_the_old_path() {
    let rig = two();
    assert!(
        rig.engine
            .provider_for_role(Role::Main, CallSource::Chat)
            .is_some()
    );
    rig.engine
        .apply_edit(ConfigEdit::SetRole {
            role: Role::Main,
            route: None,
        })
        .unwrap();
    assert!(
        rig.engine
            .provider_for_role(Role::Main, CallSource::Chat)
            .is_none()
    );
    assert_eq!(target(&rig.engine.decide(&query())), None);
}

// ── Секреты и сборка ────────────────────────────────────────────────────

#[test]
fn the_env_only_stub_reads_env_names_and_never_prints_a_value() {
    let resolver = EnvOnlyResolver;
    let account = id("a");
    let none = SecretRef::parse("none").unwrap();
    assert_eq!(resolver.resolve(&none, &account).unwrap().expose(), "");
    let missing = SecretRef::parse("env:POOPRUSTEEK_SURELY_UNSET_VARIABLE").unwrap();
    let error = resolver.resolve(&missing, &account).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("POOPRUSTEEK_SURELY_UNSET_VARIABLE")
    );
    let keyring = SecretRef::parse("keyring:global/x").unwrap();
    assert!(matches!(
        resolver.resolve(&keyring, &account),
        Err(SecretError::Unsupported { .. })
    ));
    assert_eq!(
        format!("{:?}", Secret::new("sk-live".into())),
        "Secret(***)"
    );
}

#[test]
fn an_account_whose_secret_cannot_be_resolved_is_unavailable_not_fatal() {
    let toml = TWO_ACCOUNTS.replacen(
        "secret = \"none\"",
        "secret = \"env:POOPRUSTEEK_SURELY_UNSET_VARIABLE\"",
        1,
    );
    let deps = EngineDeps {
        data_dir: None,
        clock: Arc::new(SystemClock),
        resolver: Arc::new(EnvOnlyResolver),
        factories: vec![Arc::new(BuiltinFactory::new(None))],
        persist: None,
    };
    let engine = Engine::new(parse(&toml), deps);
    let snapshot = engine.snapshot();
    assert!(matches!(
        &snapshot.accounts[0].status,
        AccountStatus::Unavailable { reason } if reason.contains("POOPRUSTEEK_SURELY_UNSET_VARIABLE")
    ));
    assert_eq!(
        snapshot.accounts[1].status,
        AccountStatus::Ready,
        "b is fine"
    );
    let decision = engine.decide(&query());
    assert_eq!(target(&decision), Some("b"));
    // Причина видна в решении: не «недоступен», а что именно не прочиталось.
    assert!(
        matches!(
            skip_of(&decision, "a"),
            Some(SkipReason::InvalidConfig { message })
                if message.contains("POOPRUSTEEK_SURELY_UNSET_VARIABLE")
        ),
        "{:?}",
        skip_of(&decision, "a")
    );
}

/// При `enabled = false` провайдер собирается ровно так, как до шлюза.
#[test]
fn a_disabled_gateway_builds_exactly_the_provider_the_old_path_builds() {
    let mut config = Config::default();
    config.provider.token = "token".to_string();
    config.provider.model = "deepseek-chat".to_string();
    config.providers.push(crate::config::ProviderEntry {
        name: "lmstudio".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key: None,
        model: "qwen".into(),
        protocol: crate::config::ProviderProtocol::Openai,
        tools: ToolProtocol::Native,
    });

    let gateway = build_gateway(
        GatewayConfig {
            accounts: vec![Account::new(id("x"), AccountProvider::Custom)],
            ..GatewayConfig::default()
        },
        EngineDeps::for_config(&config),
    );
    assert!(
        !gateway.is_enabled(),
        "enabled = false stays off whatever else is set"
    );
    assert!(gateway.snapshot().accounts.is_empty(), "nothing was built");

    for active in [None, Some("lmstudio".to_string())] {
        config.active_provider = active;
        let old = crate::provider::build_provider(&config).expect("old path builds");
        let new = main_provider(&config, Some(&gateway)).expect("same through the gateway");
        assert_eq!(new.model(), old.model());
        assert_eq!(new.native_tools(), old.native_tools());
        assert_eq!(
            new.keeps_server_side_history(),
            old.keeps_server_side_history()
        );
        assert!(
            new.route_state().is_none(),
            "no gateway state leaks into a plain provider"
        );
    }
    config.provider.token.clear();
    config.active_provider = None;
    assert!(crate::provider::build_provider(&config).is_none());
    assert!(
        main_provider(&config, Some(&gateway)).is_none(),
        "still nothing, as before"
    );
}

#[test]
fn the_config_hook_is_disabled_until_secrets_wires_it_in() {
    assert!(!gateway_config(&Config::default()).gateway.enabled);
}

// ── Проверка конфига при сборке ─────────────────────────────────────────

fn zero_slots_on_a() -> String {
    TWO_ACCOUNTS.replace(
        "model = \"model-a\"",
        "model = \"model-a\"\n[accounts.limits]\nmax_concurrency = 0",
    )
}

fn two_fakes() -> Vec<(&'static str, Arc<FakeBackend>)> {
    vec![
        ("a", FakeBackend::new(vec![])),
        ("b", FakeBackend::new(vec![])),
    ]
}

/// Ошибка конфига отключает аккаунт, а не весь шлюз; причина видна и в снимке, и в решении.
#[tokio::test]
async fn an_account_with_a_config_error_is_off_with_its_reason_and_the_rest_keeps_working() {
    let rig = rig(&zero_slots_on_a(), two_fakes());
    let snapshot = rig.engine.snapshot();
    assert!(
        snapshot
            .problems
            .iter()
            .any(|p| p.severity == Severity::Error && p.message.contains("max_concurrency")),
        "{:?}",
        snapshot.problems
    );
    assert!(matches!(
        &snapshot.accounts[0].status,
        AccountStatus::Unavailable { reason } if reason.contains("max_concurrency")
    ));
    assert_eq!(snapshot.accounts[1].status, AccountStatus::Ready);

    let decision = rig.engine.decide(&query());
    assert_eq!(target(&decision), Some("b"));
    assert!(matches!(
        skip_of(&decision, "a"),
        Some(SkipReason::InvalidConfig { message }) if message.contains("max_concurrency")
    ));
    assert!(decision.explain().contains("a: config error"));

    // Нулевой семафор не вешает вызов: ход спокойно идёт на b.
    let provider = rig.main();
    let (result, text) = super::testkit::ask(&provider, "go").await;
    result.unwrap();
    assert_eq!(text, "ok");
    assert_eq!(rig.fakes["a"].call_count(), 0);
}

#[test]
fn a_healthy_config_has_no_errors_to_report() {
    let rig = two();
    assert!(
        rig.engine
            .snapshot()
            .problems
            .iter()
            .all(|p| p.severity != Severity::Error)
    );
}

#[test]
fn a_route_that_mixes_tool_protocols_is_off_not_silently_prompt_mode() {
    let mixed = r#"
        [gateway]
        enabled = true
        [[accounts]]
        id = "a"
        provider = "custom"
        base_url = "http://a.invalid/v1"
        secret = "none"
        tools = "native"
        model = "m"
        [[accounts]]
        id = "b"
        provider = "custom"
        base_url = "http://b.invalid/v1"
        secret = "none"
        model = "m"
        [[routes]]
        id = "main"
        steps = [{ account = "a" }, { account = "b" }]
        [roles]
        main = "main"
    "#;
    let rig = rig(mixed, two_fakes());
    assert!(
        rig.engine
            .provider_for_role(Role::Main, CallSource::Chat)
            .is_none(),
        "the app stays on its old path and says why"
    );
    let decision = rig.engine.decide(&query());
    assert_eq!(decision.target, None);
    assert!(
        matches!(&decision.why, Why::ConfigInvalid { message } if message.contains("allow_mixed_tools"))
    );
    assert!(
        decision.explain().starts_with("route is off:"),
        "{}",
        decision.explain()
    );

    // Обратная сторона: с явным разрешением тот же маршрут работает, в промптовом режиме.
    let allowed = mixed.replace("id = \"main\"", "id = \"main\"\nallow_mixed_tools = true");
    let rig = self::rig(&allowed, two_fakes());
    let provider = rig
        .engine
        .provider_for_role(Role::Main, CallSource::Chat)
        .expect("allowed");
    assert!(!provider.native_tools());
}

#[test]
fn a_route_step_without_a_model_is_off_with_the_reason() {
    let no_model = TWO_ACCOUNTS.replace("    model = \"model-a\"\n", "");
    let rig = rig(&no_model, two_fakes());
    let decision = rig.engine.decide(&query());
    assert!(
        matches!(&decision.why, Why::ConfigInvalid { message } if message.contains("has no model"))
    );
    assert!(
        rig.engine
            .snapshot()
            .problems
            .iter()
            .any(|p| p.message.contains("has no model"))
    );
}

fn claude_token(provider: AccountProvider, base_url: Option<&str>, ack: bool) -> Account {
    let mut account = Account::new(id("t"), provider);
    account.base_url = base_url.map(str::to_string);
    account.direct_token = Some(SecretRef::parse("none").unwrap());
    account.direct_token_ack = ack;
    account.model = Some("m".into());
    account
}

/// `direct_token` на Claude не работает при любых условиях, даже в обход `validate`.
#[test]
fn a_direct_token_never_builds_an_adapter_for_claude() {
    let factory = BuiltinFactory::new(None);
    let build = |account: &Account| {
        factory
            .build(account, &EnvOnlyResolver)
            .expect("an API account")
    };
    for account in [
        claude_token(AccountProvider::Anthropic, None, true),
        claude_token(
            AccountProvider::Custom,
            Some("https://api.anthropic.com/v1"),
            true,
        ),
        claude_token(AccountProvider::Custom, Some("https://claude.ai/x"), true),
    ] {
        let error = build(&account).err().expect("refused");
        assert!(error.contains("not allowed for Claude"), "{error}");
    }
    // Не Claude и с подтверждением — строится; без подтверждения — нет.
    let fine = claude_token(
        AccountProvider::Custom,
        Some("https://api.example.com/v1"),
        true,
    );
    assert!(build(&fine).is_ok());
    let unacknowledged = claude_token(
        AccountProvider::Custom,
        Some("https://api.example.com/v1"),
        false,
    );
    assert!(
        build(&unacknowledged)
            .err()
            .unwrap()
            .contains("direct_token_ack")
    );
}

#[test]
fn a_direct_token_for_claude_in_a_config_leaves_the_account_off() {
    let toml = format!(
        "{}\n[[accounts]]\nid = \"claude\"\nprovider = \"anthropic\"\nmodel = \"m\"\ndirect_token = \"env:SOME_TOKEN\"\ndirect_token_ack = true\n",
        TWO_ACCOUNTS
    );
    let deps = EngineDeps {
        data_dir: None,
        clock: Arc::new(SystemClock),
        resolver: Arc::new(EnvOnlyResolver),
        factories: vec![Arc::new(BuiltinFactory::new(None))],
        persist: None,
    };
    let engine = Engine::new(parse(&toml), deps);
    let view = engine
        .snapshot()
        .accounts
        .into_iter()
        .find(|a| a.id.as_str() == "claude")
        .unwrap();
    assert!(matches!(
        view.status,
        AccountStatus::Unavailable { reason } if reason.contains("not allowed for Claude")
    ));
}

#[test]
fn old_errors_do_not_block_unrelated_edits_but_new_ones_still_do() {
    let rig = rig(&zero_slots_on_a(), two_fakes());
    rig.engine
        .apply_edit(ConfigEdit::SetLabel {
            id: id("b"),
            label: "Spare".into(),
        })
        .expect("an unrelated edit goes through while a is broken");
    // Исправить сломанное тоже можно.
    let mut repaired = parse(&zero_slots_on_a()).accounts.remove(0);
    repaired.limits.max_concurrency = Some(2);
    rig.engine
        .apply_edit(ConfigEdit::UpsertAccount(Box::new(repaired)))
        .expect("the fix applies");
    assert!(
        rig.engine
            .snapshot()
            .problems
            .iter()
            .all(|p| p.severity != Severity::Error)
    );
    assert_eq!(target(&rig.engine.decide(&query())), Some("a"), "a is back");
    // Новая ошибка по-прежнему отказывает.
    let ghost = super::types::Route {
        id: super::types::RouteId::parse("ghost").unwrap(),
        strategy: super::types::Strategy::Ordered,
        steps: vec![super::types::Step {
            account: id("nobody"),
            model: None,
        }],
        on: Vec::new(),
        switch_at_pct: None,
        allow_mixed_tools: false,
    };
    assert!(matches!(
        rig.engine
            .apply_edit(ConfigEdit::UpsertRoute(Box::new(ghost))),
        Err(EditError::Invalid(_))
    ));
}
