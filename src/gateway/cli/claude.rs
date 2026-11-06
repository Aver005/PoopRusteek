//! Claude Code в режиме модели (`06-cli-spike.md` §1): строгие флаги, поток `stream-json`.
//! Токены не читаем и не храним; лимиты берём только из `rate_limit_event` самого CLI.

use super::classify::classify_claude;
use super::dialect::{
    AuthCheck, Dialect, Failure, Feed, ModelSource, Parser, Plan, Success, Terminal, epoch_to_time,
    json_u32, require_empty_cwd,
};
use super::discover::Version;
use super::launch::{Launch, child_env, has_prefix};
use super::process::Limits;
use crate::gateway::backend::{AuthState, Capabilities, UsageSupport};
use crate::gateway::handle::{LimitSnapshot, LimitSource, LimitWindow, WindowKind};
use crate::gateway::types::{AccountId, CliTool};
use crate::provider::Usage;
use serde_json::Value;
use std::ffi::OsString;
use std::time::{Duration, SystemTime};

/// С этой версии есть `--restricted`; на старых вместо него был бы только `--safe-mode`.
pub const MIN_VERSION: Version = Version(2, 1, 248);

/// Модели, которые CLI принимает по короткому имени.
pub const MODEL_ALIASES: [&str; 3] = ["haiku", "sonnet", "opus"];

pub struct ClaudeDialect;

impl Dialect for ClaudeDialect {
    fn tool(&self) -> CliTool {
        CliTool::Claude
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_tools: false,
            pure_model: true,
            streaming: true,
            usage: UsageSupport::Reported,
            reports_limits: true,
            server_side_session: false,
            slow_start: true,
            reports_context_window: true,
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            // Дельты идут непрерывно (в том числе рассуждения): минута тишины — зависание.
            idle: Duration::from_secs(90),
            total: Duration::from_secs(30 * 60),
            exit_grace: Duration::from_millis(1500),
        }
    }

    fn launch(&self, plan: &Plan<'_>) -> Launch {
        let mut args: Vec<OsString> = plan.program.prefix.clone();
        args.extend(
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--restricted",
                "--tools",
                "",
                "--disable-slash-commands",
                "--strict-mcp-config",
                "--permission-mode",
                "dontAsk",
                "--permission-prompts",
                "none",
                "--no-session-persistence",
                "--system-prompt-file",
            ]
            .map(OsString::from),
        );
        args.push(plan.system_file.into());
        if !plan.model.is_empty() {
            args.extend(["--model".into(), plan.model.into()]);
        }
        // Чужой вход через переменные родителя и платный ключ вместо подписки не допускаем.
        let strip = |name: &str| {
            has_prefix(name, "CLAUDE_")
                || [
                    "CLAUDECODE",
                    "ANTHROPIC_API_KEY",
                    "ANTHROPIC_AUTH_TOKEN",
                    "ANTHROPIC_BASE_URL",
                ]
                .iter()
                .any(|own| name.eq_ignore_ascii_case(own))
        };
        let set = plan
            .home
            .map(|home| vec![("CLAUDE_CONFIG_DIR", home.as_os_str().to_owned())])
            .unwrap_or_default();
        Launch {
            program: plan.program.path.clone(),
            args,
            env: child_env(plan.parent_env.clone(), strip, set),
            cwd: plan.scratch.work_dir(),
            stdin: plan.prompt.clone().into_bytes(),
        }
    }

    fn verify(&self, launch: &Launch) -> Result<(), String> {
        let missing = |flag: &str| format!("claude launch lacks {flag}: refusing to start");
        for flag in [
            "--restricted",
            "--disable-slash-commands",
            "--strict-mcp-config",
        ] {
            if !launch.has_arg(flag) {
                return Err(missing(flag));
            }
        }
        // Пустое значение `--tools` — именно отсутствие инструментов, не «по умолчанию».
        if launch
            .arg_after("--tools")
            .is_none_or(|value| !value.is_empty())
        {
            return Err(missing("an empty --tools"));
        }
        if launch
            .arg_after("--permission-mode")
            .is_none_or(|v| v != "dontAsk")
        {
            return Err(missing("--permission-mode dontAsk"));
        }
        if let Some(name) = launch.env_names().into_iter().find(|name| {
            has_prefix(name, "CLAUDE_") && !name.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR")
        }) {
            return Err(format!(
                "parent variable {name} leaked into the claude environment"
            ));
        }
        require_empty_cwd(launch)
    }

    fn parser(&self, account: &AccountId) -> Box<dyn Parser> {
        Box::new(ClaudeParser::new(account.clone()))
    }

    fn auth_check(&self) -> Option<AuthCheck> {
        Some(AuthCheck {
            args: &["auth", "status"],
            parse: auth_state,
        })
    }

    fn models(&self) -> ModelSource {
        ModelSource::Aliases(&MODEL_ALIASES)
    }
}

/// `claude auth status` печатает JSON; из него нужен один булев признак.
fn auth_state(output: &str) -> AuthState {
    // Перед JSON может стоять строка-баннер: разбор начинается с первой `{`.
    let logged_in = output
        .find('{')
        .and_then(|at| serde_json::from_str::<Value>(&output[at..]).ok())
        .and_then(|v| v.get("loggedIn").and_then(Value::as_bool));
    match logged_in {
        Some(true) => AuthState::Ok,
        Some(false) => AuthState::NeedsLogin,
        None => AuthState::Unknown,
    }
}

pub struct ClaudeParser {
    account: AccountId,
    streamed: bool,
    model: Option<String>,
    limits: Option<LimitSnapshot>,
    /// `Some(сброс)` — лимит отклонён; сброс может быть неизвестен.
    rejected: Option<Option<SystemTime>>,
    terminal: Option<Terminal>,
}

impl ClaudeParser {
    pub fn new(account: AccountId) -> Self {
        Self {
            account,
            streamed: false,
            model: None,
            limits: None,
            rejected: None,
            terminal: None,
        }
    }

    fn on_rate_limit(&mut self, value: &Value) {
        let Some(info) = value.get("rate_limit_info") else {
            return;
        };
        let mut windows = Vec::new();
        let mut rejected_at: Option<Option<SystemTime>> = None;
        if let Some(map) = info.get("unifiedWindows").and_then(Value::as_object) {
            for (key, window) in map {
                let resets_at = window.get("resetsAt").and_then(epoch_to_time);
                if window.get("status").and_then(Value::as_str) == Some("rejected") {
                    rejected_at = Some(resets_at);
                }
                windows.push(LimitWindow {
                    name: window_name(key),
                    kind: WindowKind::Percent,
                    used_pct: window
                        .get("utilization")
                        .and_then(Value::as_f64)
                        .map(|fraction| (fraction * 100.0).clamp(0.0, 100.0) as f32),
                    limit: None,
                    remaining: None,
                    resets_at,
                });
            }
        }
        if info.get("status").and_then(Value::as_str) == Some("rejected") {
            rejected_at = Some(
                rejected_at
                    .flatten()
                    .or_else(|| info.get("resetsAt").and_then(epoch_to_time)),
            );
        }
        if rejected_at.is_some() {
            self.rejected = rejected_at;
        }
        if !windows.is_empty() {
            self.limits = Some(LimitSnapshot {
                account: self.account.clone(),
                plan: None,
                windows,
                observed_at: SystemTime::now(),
                source: LimitSource::CliEvent,
            });
        }
    }

    fn on_result(&mut self, value: &Value) -> Feed {
        let text = value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let subtype = value
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let failed = value.get("is_error").and_then(Value::as_bool) == Some(true)
            || subtype.starts_with("error");
        if failed {
            let status = value
                .get("api_error_status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok());
            let shown = if text.trim().is_empty() {
                format!(
                    "claude failed ({})",
                    value
                        .get("terminal_reason")
                        .and_then(Value::as_str)
                        .unwrap_or(subtype)
                )
            } else {
                text
            };
            let (class, unrecognized) = classify_claude(&shown, status, self.rejected);
            self.terminal = Some(Err(Failure {
                class,
                status,
                message: shown,
                limits: self.limits.clone(),
                unrecognized,
            }));
            return Feed::done(String::new());
        }
        let usage = value.get("usage").map(claude_usage).unwrap_or_default();
        let finish_reason = match value.get("stop_reason").and_then(Value::as_str) {
            Some("max_tokens") => "length",
            _ => "stop",
        };
        self.terminal = Some(Ok(Success {
            usage,
            finish_reason: finish_reason.to_string(),
            context_window: self.context_window(value),
            upstream_id: value
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            limits: self.limits.clone(),
        }));
        // Дельт не было (нет `--include-partial-messages` в потоке): берём готовый текст.
        let tail = if self.streamed { String::new() } else { text };
        Feed::done(tail)
    }

    fn context_window(&self, result: &Value) -> Option<u32> {
        let usage = result.get("modelUsage")?.as_object()?;
        let entry = self
            .model
            .as_deref()
            .and_then(|model| usage.get(model))
            .or_else(|| usage.values().next())?;
        entry
            .get("contextWindow")
            .and_then(Value::as_u64)
            .and_then(|window| u32::try_from(window).ok())
            .filter(|window| *window > 0)
    }
}

impl Parser for ClaudeParser {
    fn feed(&mut self, line: &str) -> Feed {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Feed::default();
        };
        if self.terminal.is_some() {
            return Feed::default();
        }
        match value.get("type").and_then(Value::as_str) {
            Some("system") => {
                if value.get("subtype").and_then(Value::as_str) == Some("init") {
                    self.model = value
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                Feed::default()
            }
            Some("stream_event") => {
                let nested = value
                    .get("parent_tool_use_id")
                    .is_some_and(|id| !id.is_null());
                let delta = value
                    .pointer("/event/delta")
                    .filter(|_| {
                        value.pointer("/event/type").and_then(Value::as_str)
                            == Some("content_block_delta")
                    })
                    .filter(|delta| delta.get("type").and_then(Value::as_str) == Some("text_delta"))
                    .and_then(|delta| delta.get("text").and_then(Value::as_str));
                match delta {
                    Some(text) if !nested && !text.is_empty() => {
                        self.streamed = true;
                        Feed::text(text.to_string())
                    }
                    _ => Feed::default(),
                }
            }
            Some("rate_limit_event") => {
                self.on_rate_limit(&value);
                Feed::default()
            }
            Some("result") => self.on_result(&value),
            _ => Feed::default(),
        }
    }

    fn terminal(&mut self) -> Option<Terminal> {
        self.terminal.take()
    }

    fn limits(&self) -> Option<LimitSnapshot> {
        self.limits.clone()
    }
}

fn window_name(key: &str) -> String {
    match key {
        "five_hour" => "5h".to_string(),
        "seven_day" => "weekly".to_string(),
        other => other.replace('_', "-"),
    }
}

/// `prompt_tokens` — весь вход вместе с кэшем; `output_tokens` у Anthropic уже с рассуждениями.
fn claude_usage(usage: &Value) -> Usage {
    let field = |name: &str| usage.get(name).filter(|v| v.is_number());
    let input = json_u32(usage.get("input_tokens"));
    let read = json_u32(usage.get("cache_read_input_tokens"));
    let write = json_u32(usage.get("cache_creation_input_tokens"));
    let output = json_u32(usage.get("output_tokens"));
    let prompt = input.saturating_add(read).saturating_add(write);
    Usage {
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: prompt.saturating_add(output),
        cache_read_tokens: field("cache_read_input_tokens").map(|_| read),
        cache_write_tokens: field("cache_creation_input_tokens").map(|_| write),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/thinking_tokens")
            .filter(|v| v.is_number())
            .map(|v| json_u32(Some(v))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::failure::FailureClass;

    fn account() -> AccountId {
        AccountId::parse("claude-a").unwrap()
    }

    /// Поток настоящего запуска (06-cli-spike §1.3), без идентификаторов сессии.
    const SUCCESS: &[&str] = &[
        r#"{"type":"system","subtype":"init","model":"claude-haiku-5-5","tools":[],"apiKeySource":"none","claude_code_version":"2.1.318"}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":2}}},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"p"}},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ong"}},"parent_tool_use_id":null}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"pong"}]}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1793990400,"rateLimitType":"five_hour","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.74,"resetsAt":1793990400,"status":"allowed"},"seven_day":{"utilization":0.19,"resetsAt":1794211200,"status":"allowed"}}}}"#,
        r#"{"stop_reason":"end_turn","usage":{"input_tokens":2,"cache_creation_input_tokens":693,"cache_read_input_tokens":10,"output_tokens":4,"output_tokens_details":{"thinking_tokens":1}},"modelUsage":{"claude-haiku-5-5":{"contextWindow":1000000,"maxOutputTokens":128000}},"is_error":false,"subtype":"success","api_error_status":null,"result":"pong","type":"result","session_id":"s-1"}"#,
    ];

    fn run(lines: &[&str]) -> (ClaudeParser, String, bool) {
        let mut parser = ClaudeParser::new(account());
        let mut text = String::new();
        let mut done = false;
        for line in lines {
            let feed = parser.feed(line);
            text.push_str(&feed.text);
            done |= feed.done;
        }
        (parser, text, done)
    }

    #[test]
    fn deltas_become_text_and_thinking_stays_out() {
        let (mut parser, text, done) = run(SUCCESS);
        assert_eq!(
            text, "pong",
            "thinking deltas and the full message are not duplicated"
        );
        assert!(done);
        assert!(parser.terminal().unwrap().is_ok());
    }

    #[test]
    fn usage_counts_the_whole_input_with_cache() {
        let (mut parser, ..) = run(SUCCESS);
        let success = parser.terminal().unwrap().unwrap();
        assert_eq!(success.usage.prompt_tokens, 2 + 693 + 10);
        assert_eq!(success.usage.completion_tokens, 4);
        assert_eq!(success.usage.total_tokens, 2 + 693 + 10 + 4);
        assert_eq!(success.usage.cache_read_tokens, Some(10));
        assert_eq!(success.usage.cache_write_tokens, Some(693));
        assert_eq!(success.usage.reasoning_tokens, Some(1));
        assert_eq!(success.finish_reason, "stop");
    }

    #[test]
    fn the_context_window_comes_from_the_model_usage() {
        let (mut parser, ..) = run(SUCCESS);
        assert_eq!(
            parser.terminal().unwrap().unwrap().context_window,
            Some(1_000_000)
        );
    }

    #[test]
    fn the_rate_limit_event_becomes_a_cli_event_snapshot() {
        let (mut parser, ..) = run(SUCCESS);
        let limits = parser.terminal().unwrap().unwrap().limits.unwrap();
        assert_eq!(limits.source, LimitSource::CliEvent);
        let names: Vec<&str> = limits.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["5h", "weekly"]);
        assert_eq!(limits.windows[0].used_pct, Some(74.0));
        assert_eq!(
            limits.windows[1].resets_at,
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(1_794_211_200))
        );
        assert!(limits.windows.iter().all(|w| w.kind == WindowKind::Percent));
    }

    #[test]
    fn without_deltas_the_final_text_is_used_once() {
        let lines = [SUCCESS[0], SUCCESS[8]];
        let (_, text, _) = run(&lines);
        assert_eq!(text, "pong");
    }

    #[test]
    fn not_logged_in_is_auth_even_though_the_subtype_says_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login","terminal_reason":"api_error","api_error_status":null,"usage":{}}"#;
        let (mut parser, text, done) = run(&[line]);
        assert!(done && text.is_empty());
        let failure = parser.terminal().unwrap().unwrap_err();
        assert_eq!(failure.class, FailureClass::Auth);
        assert!(!failure.unrecognized);
        assert!(failure.message.contains("Not logged in"));
    }

    #[test]
    fn a_rejected_window_with_an_error_means_exhausted_until_its_reset() {
        let limit = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":1.0,"resetsAt":1793990400,"status":"rejected"}}}}"#;
        let result = r#"{"type":"result","subtype":"success","is_error":true,"result":"limit text","api_error_status":429}"#;
        let (mut parser, ..) = run(&[limit, result]);
        let failure = parser.terminal().unwrap().unwrap_err();
        assert_eq!(
            failure.class,
            FailureClass::QuotaExhausted {
                resets_at: SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(1_793_990_400))
            }
        );
        assert_eq!(failure.status, Some(429));
        assert_eq!(failure.limits.unwrap().windows[0].used_pct, Some(100.0));
    }

    #[test]
    fn an_unknown_error_text_is_kept_whole_and_marked_for_the_log() {
        let line = r#"{"type":"result","is_error":true,"result":"novel failure text"}"#;
        let (mut parser, ..) = run(&[line]);
        let failure = parser.terminal().unwrap().unwrap_err();
        assert_eq!(failure.class, FailureClass::Server);
        assert!(failure.unrecognized && failure.message == "novel failure text");
    }

    #[test]
    fn non_json_lines_and_nested_agent_deltas_are_ignored() {
        let nested = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}},"parent_tool_use_id":"toolu_1"}"#;
        let (parser, text, done) = run(&["not json at all", nested, ""]);
        assert!(text.is_empty() && !done && parser.terminal.is_none());
    }

    #[test]
    fn nothing_after_the_terminal_event_changes_the_outcome() {
        let late = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"late"}}}"#;
        let (mut parser, text, _) = run(&[SUCCESS[8], late]);
        assert_eq!(text, "pong");
        assert!(parser.terminal().is_some());
    }

    #[test]
    fn auth_status_output_maps_to_the_login_state() {
        assert_eq!(
            auth_state(r#"{"loggedIn":true,"authMethod":"x"}"#),
            AuthState::Ok
        );
        assert_eq!(auth_state(r#"{"loggedIn":false}"#), AuthState::NeedsLogin);
        assert_eq!(auth_state("garbage"), AuthState::Unknown);
        assert_eq!(
            auth_state(
                "banner
{
  \"loggedIn\": true
}"
            ),
            AuthState::Ok
        );
    }
}
