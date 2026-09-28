//! Вызовы инструментов в тексте ответа модели. Форматы, доверие и план —
//! `.docs/tool-call-formats.md`.

mod accept;
mod catalog;
mod formats;
mod scan;
mod visible;

pub use catalog::ToolCatalog;
pub use visible::{StreamTextTracker, strip_thinking_only};

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    /// Идентификатор от провайдера, если вызов пришёл родным протоколом.
    /// `None` — промптовый путь, там идентификатор придумывает сам цикл.
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
    pub origin: CallOrigin,
}

impl From<&crate::provider::ToolCall> for ParsedToolCall {
    fn from(call: &crate::provider::ToolCall) -> Self {
        Self {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            origin: CallOrigin::default(),
        }
    }
}

/// Откуда вызов: от этого зависит, можно ли выполнить его без человека.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallOrigin {
    /// Разметка вызова в тексте; `None` — родной протокол провайдера.
    pub format: Option<&'static str>,
    /// Имя, как его написала модель, если каталог его поправил.
    pub written_name: Option<String>,
}

impl CallOrigin {
    /// Чужая разметка или поправленное имя: решает человек, белый список не
    /// действует.
    pub fn needs_person(&self) -> bool {
        self.format
            .is_some_and(|format| !accept::is_trusted(format))
            || self.written_name.is_some()
    }

    /// Строка для окна подтверждения: почему спрашиваем.
    pub fn warning(&self) -> Option<String> {
        if !self.needs_person() {
            return None;
        }
        Some(match (&self.written_name, self.format) {
            (Some(written), _) => format!("⚠ The model wrote the tool name as `{written}`."),
            (None, format) => format!(
                "⚠ Written in {} markup, not the agent's own <tool_use>.",
                format.unwrap_or("unknown")
            ),
        })
    }
}

/// Что разбору известно о ходе.
pub struct ParseCtx<'a> {
    pub catalog: &'a ToolCatalog,
    /// Выводы инструментов беседы: чужая разметка, взятая оттуда, не исполняется.
    pub tool_outputs: Vec<&'a str>,
    /// Подтвердить некому: `auto_approve` или суб-агент.
    pub unattended: bool,
}

/// Что вынуто из ответа модели за один проход.
#[derive(Debug, Default)]
pub struct ParsedReply {
    pub calls: Vec<ParsedToolCall>,
    /// Диагностики для повтора: битый вызов, вызов внутри рассуждений.
    pub errors: Vec<String>,
    /// Ответ без вызовов и рассуждений — то, что видит человек.
    pub visible: String,
    /// Имя инструмента внутри разметки, которую никто не разобрал.
    pub suspect: Option<String>,
    /// Ответ кончился посреди вызова, начатого с этого байта: его стоит
    /// продолжить, а не переписать.
    pub cut_at: Option<usize>,
}

/// Разобрать ответ модели. Сломанный вызов становится диагностикой, а не
/// догадкой: починка битого JSON могла бы запустить не ту команду.
pub fn parse_text(text: &str, ctx: &ParseCtx) -> ParsedReply {
    scan::run(text, ctx)
}

/// Шаг агента. Родные вызовы главнее: разбор текста — подстраховка на случай,
/// когда эндпоинт молча проглотил `tools`; сложив их, выполнили бы вызов дважды.
pub fn parse_step(raw: &str, native: &[crate::provider::ToolCall], ctx: &ParseCtx) -> ParsedReply {
    if native.is_empty() {
        return parse_text(raw, ctx);
    }
    // На родном протоколе текст — просто проза рядом с вызовом.
    ParsedReply {
        calls: native.iter().map(ParsedToolCall::from).collect(),
        visible: strip_thinking_only(raw),
        ..ParsedReply::default()
    }
}

/// Разбор без каталога и выводов — для тестов грамматик.
#[cfg(test)]
pub(crate) fn parse_reply(text: &str) -> ParsedReply {
    let catalog = ToolCatalog::default();
    parse_text(
        text,
        &ParseCtx {
            catalog: &catalog,
            tool_outputs: Vec::new(),
            unattended: false,
        },
    )
}

/// Разбор с каталогом из данных инструментов — для тестов чужих форматов:
/// без каталога их вызовы с незнакомым именем становятся прозой.
#[cfg(test)]
pub(crate) fn parse_with(text: &str, tools: &[(&str, Value)]) -> ParsedReply {
    let catalog = ToolCatalog::new(
        tools
            .iter()
            .map(|(name, schema)| (name.to_string(), schema.clone())),
    );
    parse_text(
        text,
        &ParseCtx {
            catalog: &catalog,
            tool_outputs: Vec::new(),
            unattended: false,
        },
    )
}

#[cfg(test)]
fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    parse_reply(text).calls
}

#[cfg(test)]
fn parse_tool_calls_with_errors(text: &str) -> (Vec<ParsedToolCall>, Vec<String>) {
    let reply = parse_reply(text);
    (reply.calls, reply.errors)
}

#[cfg(test)]
fn strip_tool_calls(text: &str) -> String {
    parse_reply(text).visible
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Дословно ответ из прогона `shell-reads-workspace` 2026-09-24: вызов
    /// `<tool_use>`, а следом — второй в искажённом DSML. Второй терялся.
    const MIXED_REPLY: &str = "I'll look at the directory contents and then read `Cargo.toml`.\n\n<tool_use>\n<name>powershell</name>\n<arguments>\n{\"command\":\"Get-ChildItem -Force\"}\n</arguments>\n</tool_use><｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"read_file\">\n<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">Cargo.toml</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>";

    #[test]
    fn a_dsml_call_after_a_tool_use_is_not_lost() {
        let (calls, errors) = parse_tool_calls_with_errors(MIXED_REPLY);
        assert!(errors.is_empty(), "{errors:?}");
        let names: Vec<_> = calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["powershell", "read_file"]);
        assert_eq!(calls[1].arguments["path"], "Cargo.toml");
    }

    #[test]
    fn the_canonical_dsml_shape_and_typed_parameters_parse() {
        let text = "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"edit\">\n<｜DSML｜parameter name=\"path\" string=\"true\">a.rs</｜DSML｜parameter>\n<｜DSML｜parameter name=\"count\" string=\"false\">3</｜DSML｜parameter>\n<｜DSML｜parameter name=\"opts\" string=\"false\">{\"dry\": true}</｜DSML｜parameter>\n</｜DSML｜invoke>\n<｜DSML｜invoke name=\"read_file\">\n<｜DSML｜parameter name=\"path\" string=\"true\">b.rs</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜function_calls>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a.rs");
        assert_eq!(calls[0].arguments["count"], 3);
        assert_eq!(calls[0].arguments["opts"]["dry"], true);
        assert_eq!(calls[1].name, "read_file");
    }

    /// `string="true"` не трогается: код с отступами и переводами строк
    /// приходит ровно как написан.
    #[test]
    fn a_string_parameter_keeps_its_exact_text() {
        let text = "<｜DSML｜invoke name=\"write\"><｜DSML｜parameter name=\"content\" string=\"true\">\n  fn x() {}\n</｜DSML｜parameter></｜DSML｜invoke>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls[0].arguments["content"], "\n  fn x() {}\n");
    }

    #[test]
    fn a_cut_off_dsml_invoke_is_reported_not_dropped() {
        let text = "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"read_file\">\n<｜DSML｜parameter name=\"path\" string=\"true\">a</｜DSML｜parameter>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(calls.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn a_call_cut_at_the_end_is_flagged_for_continuation() {
        let cut = "Writing it.\n<tool_use><name>write</name><arguments>{\"path\": \"a.rs\", \"content\": \"fn ma";
        assert_eq!(parse_reply(cut).cut_at, Some("Writing it.\n".len()));
        let dsml = "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"write\">\n<｜DSML｜parameter name=\"content\" string=\"true\">fn ma";
        assert_eq!(parse_reply(dsml).cut_at, Some(0));
    }

    /// Битый JSON посреди ответа — ошибка для повтора, а не обрыв.
    #[test]
    fn a_broken_call_followed_by_text_is_not_cut_off() {
        let text = "<tool_use><name>bash</name><arguments>{\"command\": \"a \"b\"\"}</arguments></tool_use>\nDone.";
        let reply = parse_reply(text);
        assert!(!reply.errors.is_empty());
        assert_eq!(reply.cut_at, None);
        assert_eq!(parse_reply("Just prose.").cut_at, None);
    }

    #[test]
    fn dsml_markup_never_reaches_the_shown_answer() {
        let shown = strip_tool_calls(MIXED_REPLY);
        assert_eq!(
            shown,
            "I'll look at the directory contents and then read `Cargo.toml`."
        );
    }

    /// Модель написала DSML раньше `<tool_use>` — так и выполняем.
    #[test]
    fn calls_run_in_the_order_they_were_written() {
        let text = "<｜DSML｜invoke name=\"first\"></｜DSML｜invoke>\n[TOOL:second] {}\n<tool_use><name>third</name><arguments>{}</arguments></tool_use>";
        let names: Vec<_> = parse_tool_calls(text).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    /// Искажённая веб-обёртка с двумя вызовами, ASCII-черты, вызов без
    /// параметров, `string="false"` с негодным JSON и без атрибута вовсе.
    #[test]
    fn tolerant_shapes_all_parse() {
        let text = "<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"a\">\n<｜｜DSML｜｜ parameter name=\" n \" string=\"false\">not json</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n<|DSML|invoke name=\"b\"><|DSML|parameter name=\"k\">42</|DSML|parameter></|DSML|invoke>\n<｜DSML｜invoke name=\"c\"></｜DSML｜invoke>\n</｜｜DSML｜｜ calls>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(calls[0].arguments["n"], "not json");
        assert_eq!(calls[1].arguments["k"], 42);
        assert_eq!(calls[2].arguments, serde_json::json!({}));
    }

    /// Тот же вызов дважды — `<tool_use>` и следом DSML — выполняется раз.
    #[test]
    fn a_dsml_echo_of_a_tool_use_runs_once() {
        let text = "<tool_use><name>bash</name><arguments>{\"command\": \"echo 1 >> log\"}</arguments></tool_use><｜DSML｜invoke name=\"bash\"><｜DSML｜parameter name=\"command\" string=\"true\">echo 1 >> log</｜DSML｜parameter></｜DSML｜invoke>";
        assert_eq!(parse_tool_calls(text).len(), 1);
    }

    /// `<tool_use>` внутри аргумента DSML-вызова — это текст для записи.
    #[test]
    fn a_tool_use_inside_a_dsml_argument_is_not_a_second_call() {
        let text = "<｜DSML｜invoke name=\"write\"><｜DSML｜parameter name=\"content\" string=\"true\">see <tool_use><name>bash</name><arguments>{\"command\":\"rm -rf x\"}</arguments></tool_use></｜DSML｜parameter></｜DSML｜invoke>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write");
    }

    /// Процитированный тег в объяснении не делает ответ «битым вызовом».
    #[test]
    fn a_quoted_opener_is_not_reported_as_cut_off() {
        let text =
            "Each call starts with `<｜DSML｜invoke name=\"tool\">` and ends with a closing tag.";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(calls.is_empty() && errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_bare_invoke_is_stripped_from_the_shown_answer() {
        let text = "Reading it now.\n<｜DSML｜invoke name=\"read_file\"><｜DSML｜parameter name=\"path\" string=\"true\">a</｜DSML｜parameter></｜DSML｜invoke>";
        assert_eq!(strip_tool_calls(text), "Reading it now.");
    }

    /// Обычный текст про DSML — не вызов.
    #[test]
    fn prose_mentioning_dsml_is_left_alone() {
        let text = "DeepSeek's DSML format wraps calls in invoke tags.";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(calls.is_empty() && errors.is_empty());
        assert_eq!(strip_tool_calls(text), text);
    }

    #[test]
    fn test_parse_simple_tool_call() {
        let text = r#"Here is the result: [TOOL:bash] {"command": "ls -la"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn test_parse_mcp_tool_call() {
        let text =
            r#"[TOOL:mcp__github__create_issue] {"title": "Bug report", "body": "Found a bug"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "mcp__github__create_issue");
        assert_eq!(calls[0].arguments["title"], "Bug report");
    }

    #[test]
    fn test_no_tool_calls() {
        let text = "This is just a regular response without any tools.";
        assert!(parse_tool_calls(text).is_empty());
    }

    #[test]
    fn test_multiple_tool_calls() {
        let text = r#"[TOOL:bash] {"command": "pwd"}
Then [TOOL:file.read] {"path": "test.txt"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn test_parse_xml_tool_call() {
        let text = r#"
<tool_use>
<name>powershell</name>
<arguments>
{"command":"Get-Location"}
</arguments>
</tool_use>
"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(calls[0].arguments["command"], "Get-Location");
    }

    /// The exact failure from the frozen-agent report: the model swapped the
    /// `</tool_use>` and `</arguments>` closing tags. Old parser dropped it
    /// with "Failed to parse <tool_use> body"; now the arguments are recovered.
    #[test]
    fn test_parse_xml_tool_call_with_swapped_closing_tags() {
        let text = "<tool_use>\n<name>powershell</name>\n<arguments>\n{\"command\":\"Get-Location\"}\n</tool_use>\n</arguments>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1, "errors: {errors:?}");
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(calls[0].arguments["command"], "Get-Location");
    }

    /// Genuinely broken JSON (unescaped quotes around a Windows path, the other
    /// half of the report) must NOT be silently dropped — it becomes an error
    /// the runner can feed back, and yields zero calls (never a wrong command).
    #[test]
    fn test_malformed_json_arguments_report_error_not_call() {
        let text = "<tool_use>\n<name>powershell</name>\n<arguments>\n{\"command\": \"Start-Process \"C:\\Users\\Aver\\x.png\"\"}\n</arguments>\n</tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(
            calls.is_empty(),
            "should not fabricate a call from bad JSON"
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("powershell"));
        assert!(errors[0].contains("not valid JSON"));
    }

    /// A dropped `<arguments>` wrapper still recovers the JSON object.
    #[test]
    fn test_parse_xml_tool_call_without_arguments_wrapper() {
        let text = "<tool_use>\n<name>bash</name>\n{\"command\":\"ls\"}\n</tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1, "errors: {errors:?}");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls");
    }

    /// The bare-JSON `{"name":…,"arguments":…}` shape (no XML tags) parses too.
    #[test]
    fn test_parse_bare_json_name_arguments_shape() {
        let text = "<tool_use>{\"name\":\"bash\",\"arguments\":{\"command\":\"pwd\"}}</tool_use>";
        let (calls, _errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "pwd");
    }

    /// Лишняя `}` перед закрывающими тегами — концовка склеенного вызова из
    /// живого прогона. Прочий мусор там по-прежнему ошибка.
    #[test]
    fn stray_closing_braces_before_the_closers_are_forgiven() {
        let text =
            "<tool_use><name>bash</name><arguments>{\"command\":\"ls\"}}</arguments>\n</tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(calls[0].arguments, serde_json::json!({"command": "ls"}));
        let junk =
            "<tool_use><name>bash</name><arguments>{\"command\":\"ls\"}} x</arguments></tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(junk);
        assert!(calls.is_empty());
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_well_formed_call_produces_no_errors() {
        let text =
            "<tool_use><name>bash</name><arguments>{\"command\":\"ls\"}</arguments></tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_strip_tool_calls() {
        let text = "Before\n<tool_use><name>bash</name><arguments>{\"command\":\"pwd\"}</arguments></tool_use>\nAfter";
        assert_eq!(strip_tool_calls(text), "Before\n\nAfter");
    }
}
