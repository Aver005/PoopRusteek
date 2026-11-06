//! OpenCode в режиме модели (`06-cli-spike.md` §2): `--standalone`, пустой конфиг, запрет инструментов.
//! Потока токенов и лимитов у него нет: чего нет, того не выдумываем.

use super::classify::classify_opencode;
use super::dialect::{
    Dialect, Failure, Feed, ModelSource, Parser, Plan, Success, Terminal, attached_value,
    check_model, count_arg, json_u32, require_empty_cwd, require_exact, require_once, value_after,
};
use super::launch::{Launch, child_env, forward_slashes, has_prefix};
use super::process::Limits;
use crate::gateway::backend::{Capabilities, UsageSupport};
use crate::gateway::handle::LimitSnapshot;
use crate::gateway::types::{AccountId, CliTool};
use crate::provider::Usage;
use crate::provider::failure::FailureClass;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::time::Duration;

/// Имя нашего агента без инструментов; `--agent` указывает на него.
const AGENT: &str = "plain";

pub struct OpencodeDialect;

/// Общий вход: эти каталоги OpenCode берёт из окружения пользователя, как есть.
const SHARED_XDG: [&str; 3] = ["XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"];

/// Все `XDG_*`, которые вообще бывают у ребёнка.
const ALLOWED_XDG: [&str; 5] = [
    "XDG_CONFIG_HOME",
    "XDG_CONFIG_DIRS",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
];

/// Аргументы режима модели; модель слитно, `--model=<m>`.
fn own_args(model: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["run", "--format", "json", "--standalone", "--agent", AGENT]
        .map(OsString::from)
        .to_vec();
    if !model.is_empty() {
        args.push(format!("--model={model}").into());
    }
    args
}

/// Папка конфигурации пуста и единственная, `OPENCODE_*` и посторонних `XDG_*` нет.
fn verify_env(launch: &Launch) -> Result<(), String> {
    let names = launch.env_names();
    if let Some(name) = names.iter().find(|name| {
        (has_prefix(name, "XDG_") && !ALLOWED_XDG.iter().any(|ok| name.eq_ignore_ascii_case(ok)))
            || (has_prefix(name, "OPENCODE_")
                && !name.eq_ignore_ascii_case("OPENCODE_CONFIG_CONTENT"))
    }) {
        return Err(format!(
            "parent variable {name} leaked into the opencode environment"
        ));
    }
    let missing = |what: &str| format!("opencode launch lacks {what}: refusing to start");
    let config_home = launch
        .env_value("XDG_CONFIG_HOME")
        .ok_or_else(|| missing("XDG_CONFIG_HOME"))?;
    if launch.env_value("XDG_CONFIG_DIRS") != Some(config_home) {
        return Err(missing("XDG_CONFIG_DIRS equal to the empty config folder"));
    }
    if std::fs::read_dir(config_home).is_ok_and(|mut entries| entries.next().is_some()) {
        return Err("XDG_CONFIG_HOME is not empty: user config would leak in".to_string());
    }
    Ok(())
}

/// Конфиг, передаваемый переменной `OPENCODE_CONFIG_CONTENT`. Промпт — файлом (`{file:…}`),
/// чтобы длинный системный текст не упёрся в предел переменной окружения.
fn config_content(system_file: &std::path::Path) -> String {
    json!({
        "permission": {"*": "deny", "skill": "deny"},
        "agent": {
            AGENT: {
                "mode": "primary",
                "description": "plain",
                "prompt": format!("{{file:{}}}", forward_slashes(system_file)),
                "permission": {"*": "deny"},
            }
        }
    })
    .to_string()
}

impl Dialect for OpencodeDialect {
    fn tool(&self) -> CliTool {
        CliTool::Opencode
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_tools: false,
            pure_model: true,
            streaming: false,
            usage: UsageSupport::Reported,
            reports_limits: false,
            server_side_session: false,
            slow_start: true,
            reports_context_window: false,
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            // Текст приходит целиком в конце шага: долгая генерация молчит, сторож щедрее.
            idle: Duration::from_secs(300),
            total: Duration::from_secs(30 * 60),
            exit_grace: Duration::from_millis(1500),
        }
    }

    fn launch(&self, plan: &Plan<'_>) -> Launch {
        let mut args: Vec<OsString> = plan.program.prefix.clone();
        let prefix_len = args.len();
        args.extend(own_args(plan.model));
        let config = plan.scratch.config_dir().into_os_string();
        let mut set: Vec<(&str, OsString)> = vec![
            ("XDG_CONFIG_HOME", config.clone()),
            // Системные каталоги конфигов (unix) подменяем той же пустой папкой.
            ("XDG_CONFIG_DIRS", config),
            (
                "OPENCODE_CONFIG_CONTENT",
                config_content(plan.system_file).into(),
            ),
        ];
        match plan.home {
            // Свой вход: данные, кэш и состояние только в папке аккаунта.
            Some(home) => set.extend([
                ("XDG_DATA_HOME", home.as_os_str().to_owned()),
                ("XDG_CACHE_HOME", home.join("xdg-cache").into_os_string()),
                ("XDG_STATE_HOME", home.join("xdg-state").into_os_string()),
            ]),
            // Общий вход: там, где пользователь держит свои данные, — ровно как у него в окружении.
            None => {
                for name in SHARED_XDG {
                    if let Some((_, value)) = plan
                        .parent_env
                        .iter()
                        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
                    {
                        set.push((name, value.clone()));
                    }
                }
            }
        }
        Launch {
            program: plan.program.path.clone(),
            args,
            prefix_len,
            // Свои `OPENCODE_*` и любые `XDG_*` родителя в режим модели не пускаем.
            env: child_env(
                plan.parent_env.clone(),
                |name| has_prefix(name, "OPENCODE_") || has_prefix(name, "XDG_"),
                set,
            ),
            cwd: plan.scratch.work_dir(),
            stdin: plan.prompt.clone().into_bytes(),
        }
    }

    fn verify(&self, launch: &Launch) -> Result<(), String> {
        let own = launch.own_args();
        let missing = |what: &str| format!("opencode launch lacks {what}: refusing to start");
        // Без `--standalone` команда уходит в службу пользователя и обходит запреты.
        require_once(own, "opencode", &["--standalone", "--agent", "run"])?;
        if value_after(own, "--agent").is_none_or(|agent| agent != AGENT) {
            return Err(missing("--agent plain"));
        }
        if count_arg(own, "--model") + count_arg(own, "-m") > 0 {
            return Err(
                "opencode launch passes the model as a separate argument: refusing to start"
                    .to_string(),
            );
        }
        let model = attached_value(own, "--model")?.unwrap_or_default();
        check_model(&model)?;
        require_exact(own, &own_args(&model), "opencode")?;
        let config: Value = launch
            .env_value("OPENCODE_CONFIG_CONTENT")
            .and_then(|raw| serde_json::from_str(&raw.to_string_lossy()).ok())
            .ok_or_else(|| missing("OPENCODE_CONFIG_CONTENT"))?;
        let denied = config.pointer("/permission/*").and_then(Value::as_str) == Some("deny")
            && config
                .pointer(&format!("/agent/{AGENT}/permission/*"))
                .and_then(Value::as_str)
                == Some("deny");
        if !denied {
            return Err(missing("a deny-all permission block"));
        }
        verify_env(launch)?;
        require_empty_cwd(launch)
    }

    fn verify_side(&self, launch: &Launch, command: &[&str]) -> Result<(), String> {
        let expected: Vec<OsString> = command.iter().map(OsString::from).collect();
        require_exact(launch.own_args(), &expected, "opencode")?;
        // Побочная команда тоже ходит со своей изоляцией, а не через службу пользователя.
        require_once(launch.own_args(), "opencode", &["--standalone"])?;
        verify_env(launch)?;
        require_empty_cwd(launch)
    }

    fn parser(&self, _account: &AccountId) -> Box<dyn Parser> {
        Box::new(OpencodeParser::new())
    }

    fn models(&self) -> ModelSource {
        ModelSource::Command(&["models", "--standalone"])
    }
}

#[derive(Default)]
pub struct OpencodeParser {
    /// Текст текущего шага: шаг с вызовом инструмента отбрасывается.
    step_text: String,
    usage: Usage,
    session: Option<String>,
    terminal: Option<Terminal>,
}

impl OpencodeParser {
    pub fn new() -> Self {
        Self::default()
    }

    fn add_tokens(&mut self, tokens: &Value) {
        let input = json_u32(tokens.get("input"));
        let output = json_u32(tokens.get("output"));
        let reasoning = json_u32(tokens.get("reasoning"));
        let read = json_u32(tokens.pointer("/cache/read"));
        let write = json_u32(tokens.pointer("/cache/write"));
        let usage = &mut self.usage;
        usage.prompt_tokens = usage.prompt_tokens.saturating_add(input + read + write);
        usage.completion_tokens = usage.completion_tokens.saturating_add(output + reasoning);
        usage.total_tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
        usage.cache_read_tokens = Some(usage.cache_read_tokens.unwrap_or(0).saturating_add(read));
        usage.cache_write_tokens =
            Some(usage.cache_write_tokens.unwrap_or(0).saturating_add(write));
        usage.reasoning_tokens = Some(
            usage
                .reasoning_tokens
                .unwrap_or(0)
                .saturating_add(reasoning),
        );
    }

    fn on_step_finish(&mut self, part: &Value) -> Feed {
        if let Some(tokens) = part.get("tokens") {
            self.add_tokens(tokens);
        }
        let reason = part.get("reason").and_then(Value::as_str);
        if reason == Some("tool-calls") {
            // Нативный вызов несуществующего инструмента (спайк §2.2): ничего не исполнено,
            // текст этого шага не наш ответ, итог придёт со следующим шагом.
            self.step_text.clear();
            return Feed::default();
        }
        let mut note = None;
        let finish_reason = match reason {
            Some("stop") => "stop",
            Some("length") => "length",
            // Неизвестная или отсутствующая причина — не успех: без текста это отказ протокола,
            // с текстом ответ оставляем и помечаем в журнале.
            other => {
                let shown = other.unwrap_or("<none>");
                if self.step_text.trim().is_empty() {
                    self.terminal = Some(Err(Failure {
                        class: FailureClass::Protocol,
                        status: None,
                        message: format!(
                            "opencode ended a step with the reason '{shown}' and no text"
                        ),
                        limits: None,
                        unrecognized: true,
                    }));
                    return Feed::done(String::new());
                }
                note = Some(format!(
                    "opencode ended the step with the unknown reason '{shown}'; the text was kept"
                ));
                "stop"
            }
        };
        self.terminal = Some(Ok(Success {
            usage: std::mem::take(&mut self.usage),
            finish_reason: finish_reason.to_string(),
            context_window: None,
            upstream_id: self.session.clone(),
            limits: None,
            note,
        }));
        Feed::done(std::mem::take(&mut self.step_text))
    }

    fn on_error(&mut self, error: &Value) -> Feed {
        let text_of = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let kind = [text_of(error, "type"), text_of(error, "name")]
            .into_iter()
            .find(|k| !k.is_empty())
            .unwrap_or_default();
        let message = [
            text_of(error, "message"),
            error
                .get("data")
                .map(|d| text_of(d, "message"))
                .unwrap_or_default(),
        ]
        .into_iter()
        .find(|m| !m.is_empty())
        .unwrap_or_else(|| error.to_string());
        let (class, unrecognized) = classify_opencode(&kind, &message);
        let shown = if kind.is_empty() {
            message
        } else {
            format!("{kind}: {message}")
        };
        self.terminal = Some(Err(Failure {
            class,
            status: None,
            message: shown,
            limits: None,
            unrecognized,
        }));
        Feed::done(String::new())
    }
}

impl Parser for OpencodeParser {
    fn feed(&mut self, line: &str) -> Feed {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Feed::default();
        };
        if self.terminal.is_some() {
            return Feed::default();
        }
        if self.session.is_none() {
            self.session = value
                .get("sessionID")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let part = value.get("part").unwrap_or(&Value::Null);
        match value.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    self.step_text.push_str(text);
                }
                Feed::default()
            }
            Some("step_finish") => self.on_step_finish(part),
            Some("error") => self.on_error(value.get("error").unwrap_or(&Value::Null)),
            Some("tool_use") => {
                // Запрет обязан был отказать любому инструменту; исполненный — нарушение изоляции.
                if part.pointer("/state/status").and_then(Value::as_str) == Some("completed") {
                    self.terminal = Some(Err(Failure {
                        class: FailureClass::BadRequest,
                        status: None,
                        message: "opencode ran a tool of its own although all were denied"
                            .to_string(),
                        limits: None,
                        unrecognized: true,
                    }));
                    return Feed::done(String::new());
                }
                Feed::default()
            }
            _ => Feed::default(),
        }
    }

    fn terminal(&mut self) -> Option<Terminal> {
        self.terminal.take()
    }

    fn limits(&self) -> Option<LimitSnapshot> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> (OpencodeParser, String, bool) {
        let mut parser = OpencodeParser::new();
        let (mut text, mut done) = (String::new(), false);
        for line in lines {
            let feed = parser.feed(line);
            text.push_str(&feed.text);
            done |= feed.done;
        }
        (parser, text, done)
    }

    /// Образец настоящего запуска (спайк §2.3): сессия вычищена.
    const SUCCESS: &[&str] = &[
        r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
        r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"pong"}}"#,
        r#"{"type":"step_finish","sessionID":"ses_1","part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"input":561,"output":3,"reasoning":25,"cache":{"read":2,"write":1}}}}"#,
    ];

    #[test]
    fn text_arrives_whole_at_the_final_step() {
        let (mut parser, text, done) = run(SUCCESS);
        assert_eq!(text, "pong");
        assert!(done);
        let success = parser.terminal().unwrap().unwrap();
        assert_eq!(success.upstream_id.as_deref(), Some("ses_1"));
        assert_eq!(success.finish_reason, "stop");
        assert!(success.limits.is_none() && success.context_window.is_none());
    }

    #[test]
    fn usage_is_the_sum_of_steps_with_cache_in_the_input() {
        let (mut parser, ..) = run(SUCCESS);
        let usage = parser.terminal().unwrap().unwrap().usage;
        assert_eq!(usage.prompt_tokens, 561 + 2 + 1);
        assert_eq!(usage.completion_tokens, 3 + 25);
        assert_eq!(usage.total_tokens, 561 + 2 + 1 + 3 + 25);
        assert_eq!(usage.reasoning_tokens, Some(25));
    }

    #[test]
    fn a_native_call_to_a_missing_tool_costs_a_step_but_not_the_answer() {
        let lines = [
            r#"{"type":"step_start","sessionID":"s"}"#,
            r#"{"type":"text","part":{"text":"I will write the file"}}"#,
            r#"{"type":"tool_use","part":{"tool":"write_file","state":{"status":"error","error":"No tool named \"write_file\" is currently available"}}}"#,
            r#"{"type":"step_finish","part":{"reason":"tool-calls","tokens":{"input":10,"output":2}}}"#,
            r#"{"type":"step_start"}"#,
            r#"{"type":"text","part":{"text":"I have no tools."}}"#,
            r#"{"type":"step_finish","part":{"reason":"stop","tokens":{"input":20,"output":4}}}"#,
        ];
        let (mut parser, text, done) = run(&lines);
        assert_eq!(text, "I have no tools.");
        assert!(done);
        assert_eq!(parser.terminal().unwrap().unwrap().usage.prompt_tokens, 30);
    }

    fn step(reason: Option<&str>, text: &str) -> Vec<String> {
        let finish = match reason {
            Some(r) => format!(
                r#"{{"type":"step_finish","part":{{"reason":"{r}","tokens":{{"input":5,"output":1}}}}}}"#
            ),
            None => {
                r#"{"type":"step_finish","part":{"tokens":{"input":5,"output":1}}}"#.to_string()
            }
        };
        let mut lines = vec![r#"{"type":"step_start"}"#.to_string()];
        if !text.is_empty() {
            lines.push(format!(r#"{{"type":"text","part":{{"text":"{text}"}}}}"#));
        }
        lines.push(finish);
        lines
    }

    fn run_owned(lines: &[String]) -> (OpencodeParser, String, bool) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        run(&refs)
    }

    #[test]
    fn an_unknown_or_missing_step_reason_without_text_is_a_protocol_failure() {
        for reason in [Some("error"), Some("aborted"), Some("other"), None] {
            let (mut parser, text, done) = run_owned(&step(reason, ""));
            assert!(done && text.is_empty(), "{reason:?}");
            let failure = parser.terminal().unwrap().unwrap_err();
            assert_eq!(failure.class, FailureClass::Protocol, "{reason:?}");
            assert!(failure.message.contains("no text"), "{}", failure.message);
        }
    }

    #[test]
    fn an_unknown_step_reason_with_text_keeps_the_text_and_leaves_a_note() {
        let (mut parser, text, _) = run_owned(&step(Some("error"), "partial but real"));
        assert_eq!(text, "partial but real");
        let success = parser.terminal().unwrap().unwrap();
        assert_eq!(success.finish_reason, "stop");
        assert!(success.note.unwrap().contains("'error'"));
    }

    #[test]
    fn the_known_reasons_stay_plain_successes_without_a_note() {
        for (reason, expected) in [("stop", "stop"), ("length", "length")] {
            let (mut parser, text, _) = run_owned(&step(Some(reason), "ok"));
            assert_eq!(text, "ok");
            let success = parser.terminal().unwrap().unwrap();
            assert_eq!(success.finish_reason, expected);
            assert!(success.note.is_none());
        }
        // Пустой ответ с честной причиной `stop` по-прежнему решает рантайм, не парсер.
        let (mut parser, ..) = run_owned(&step(Some("stop"), ""));
        assert!(parser.terminal().unwrap().is_ok());
    }

    #[test]
    fn a_tool_that_actually_ran_is_an_isolation_failure() {
        let line = r#"{"type":"tool_use","part":{"state":{"status":"completed"}}}"#;
        let (mut parser, _, done) = run(&[line]);
        assert!(done);
        let failure = parser.terminal().unwrap().unwrap_err();
        assert_eq!(failure.class, FailureClass::BadRequest);
    }

    #[test]
    fn the_verified_no_route_error_is_auth() {
        let line = r#"{"type":"error","error":{"type":"provider.no-route","message":"Model unavailable: x"}}"#;
        let (mut parser, text, done) = run(&[line]);
        assert!(done && text.is_empty());
        let failure = parser.terminal().unwrap().unwrap_err();
        assert_eq!(failure.class, FailureClass::Auth);
        assert!(!failure.unrecognized);
        assert!(failure.message.contains("Model unavailable"));
    }

    #[test]
    fn an_unknown_error_keeps_its_text_and_falls_back_to_server() {
        let line = r#"{"type":"error","error":{"name":"Weird","data":{"message":"never seen"}}}"#;
        let (mut parser, ..) = run(&[line]);
        let failure = parser.terminal().unwrap().unwrap_err();
        assert_eq!(failure.class, FailureClass::Server);
        assert!(failure.unrecognized && failure.message == "Weird: never seen");
    }

    #[test]
    fn config_content_is_valid_json_with_a_deny_all_block_and_a_file_prompt() {
        let config: Value =
            serde_json::from_str(&config_content(std::path::Path::new("C:\\a b\\system.md")))
                .unwrap();
        assert_eq!(config["permission"]["*"], "deny");
        assert_eq!(config["permission"]["skill"], "deny");
        assert_eq!(config["agent"][AGENT]["permission"]["*"], "deny");
        assert_eq!(config["agent"][AGENT]["prompt"], "{file:C:/a b/system.md}");
    }
}
