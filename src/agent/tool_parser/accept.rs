//! Приёмка находок: имя по каталогу, типы по схеме, доверие, эхо, дубли.

use super::catalog::{Resolved, coerce, property};
use super::formats::{Declared, Found, RawArgs, RawCall};
use super::{CallOrigin, ParseCtx, ParsedToolCall};
use regex::Regex;
use serde_json::Value;
use std::ops::Range;
use std::sync::LazyLock;

/// Исполняются как есть: наш формат и родные форматы DeepSeek.
const TRUSTED: &[&str] = &["tool_use", "legacy", "dsml", "deepseek_v3", "deepseek_v31"];

pub(super) fn is_trusted(format: &str) -> bool {
    TRUSTED.contains(&format)
}

/// Находка после приёмки. Пустых не бывает: находка без единого вызова или
/// ошибки — проза.
pub(super) struct Judged {
    pub span: Range<usize>,
    pub items: Vec<Result<ParsedToolCall, String>>,
}

pub(super) fn judge(text: &str, found: Found, ctx: &ParseCtx) -> Option<Judged> {
    let snippet = &text[found.span.clone()];
    let items: Vec<_> = found
        .items
        .into_iter()
        .filter_map(|item| match item {
            Err(error) => Some(Err(error)),
            Ok(raw) => verdict(snippet, raw, ctx),
        })
        .collect();
    (!items.is_empty()).then_some(Judged {
        span: found.span,
        items,
    })
}

/// `None` — чужая разметка с незнакомым именем: чей-то пример, а не вызов.
fn verdict(snippet: &str, raw: RawCall, ctx: &ParseCtx) -> Option<Result<ParsedToolCall, String>> {
    let trusted = is_trusted(raw.format);
    let (name, written_name) = match ctx.catalog.resolve(&raw.name) {
        Resolved::Exact(name) => (name.to_string(), None),
        Resolved::Renamed(name) => (name.to_string(), Some(raw.name.clone())),
        // Своему формату верим и с незнакомым именем: исполнитель ответит ошибкой.
        Resolved::Unknown if trusted => (raw.name.clone(), None),
        Resolved::Unknown => return None,
    };
    let format = raw.format;
    if !trusted
        && ctx
            .tool_outputs
            .iter()
            .any(|output| output.contains(snippet))
    {
        return Some(Err(format!(
            "the `{name}` call in {format} markup is a verbatim copy of a tool's output, so it \
             was not run. If you mean to run it, send it as {CANONICAL}."
        )));
    }
    if ctx.unattended && !trusted {
        return Some(Err(format!(
            "the `{name}` call is written in {format} markup, which runs only with a person's \
             approval, and nobody can approve it here. Re-send it as {CANONICAL}."
        )));
    }
    if ctx.unattended
        && let Some(written) = &written_name
    {
        return Some(Err(format!(
            "tool `{written}` does not exist — did you mean `{name}`? Re-send the call with the \
             exact name."
        )));
    }
    let schema = ctx.catalog.schema(&name);
    let arguments = unwrap_wrapper(arguments(raw.args, schema), schema);
    Some(Ok(ParsedToolCall {
        id: None,
        name,
        arguments,
        origin: CallOrigin {
            format: Some(format),
            written_name,
        },
    }))
}

const CANONICAL: &str =
    "<tool_use><name>TOOL</name><arguments>{ valid JSON }</arguments></tool_use>";

fn arguments(args: RawArgs, schema: Option<&Value>) -> Value {
    match args {
        // Аргументы строкой (как в ответе OpenAI) — тоже JSON.
        RawArgs::Json(Value::String(text)) => serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or(Value::String(text)),
        RawArgs::Json(value) => value,
        RawArgs::Params(params) => Value::Object(
            params
                .into_iter()
                .map(|param| {
                    let value = match param.declared {
                        Declared::Text => Value::String(param.text),
                        // Негодный JSON остаётся строкой, как у vLLM.
                        Declared::Json => serde_json::from_str(param.text.trim())
                            .unwrap_or(Value::String(param.text)),
                        Declared::BySchema => coerce(&param.text, property(schema, &param.name)),
                    };
                    (param.name, value)
                })
                .collect(),
        ),
    }
}

/// Единственный параметр `arguments`/`input`, которого нет в схеме, — обёртка
/// настоящих аргументов (vLLM `_unwrap_wrapper_args`).
fn unwrap_wrapper(args: Value, schema: Option<&Value>) -> Value {
    let Some(schema) = schema else {
        return args;
    };
    let Some((key, inner)) = args
        .as_object()
        .filter(|map| map.len() == 1)
        .and_then(|map| map.iter().next())
    else {
        return args;
    };
    if !matches!(key.as_str(), "arguments" | "input") || property(Some(schema), key).is_some() {
        return args;
    }
    let inner = match inner {
        Value::String(text) => serde_json::from_str(text).ok(),
        other => Some(other.clone()),
    };
    match inner {
        Some(object @ Value::Object(_)) => object,
        _ => args,
    }
}

/// Слабый формат (pythonic, голый JSON) — весь ответ целиком. Всё или ничего:
/// имя каждого вызова из каталога, обязательные поля схемы на месте.
pub(super) fn judge_weak(
    reply: &str,
    raws: Vec<RawCall>,
    ctx: &ParseCtx,
) -> Option<Vec<Result<ParsedToolCall, String>>> {
    let mut items = Vec::new();
    for raw in raws {
        let item = verdict(reply, raw, ctx)?;
        if let Ok(call) = &item
            && !has_required(call, ctx.catalog.schema(&call.name))
        {
            return None;
        }
        items.push(item);
    }
    Some(items)
}

fn has_required(call: &ParsedToolCall, schema: Option<&Value>) -> bool {
    let required = schema
        .and_then(|schema| schema.get("required"))
        .and_then(Value::as_array);
    required.into_iter().flatten().all(|field| {
        field
            .as_str()
            .is_some_and(|field| call.arguments.get(field).is_some())
    })
}

/// Вызовы по порядку и ошибки. Модель порой повторяет только что написанный
/// вызов в другом формате — запись или shell дважды не безобидны.
pub(super) fn collect(found: Vec<Judged>) -> (Vec<ParsedToolCall>, Vec<String>) {
    let mut calls: Vec<ParsedToolCall> = Vec::new();
    let mut errors = Vec::new();
    for item in found.into_iter().flat_map(|judged| judged.items) {
        match item {
            Err(error) => errors.push(error),
            Ok(call) => {
                let echo = calls.last().is_some_and(|last| {
                    last.origin.format != call.origin.format
                        && last.name == call.name
                        && last.arguments == call.arguments
                });
                if !echo {
                    calls.push(call);
                }
            }
        }
    }
    (calls, errors)
}

/// Что стоит прямо перед именем инструмента в разметке вызова: открытый тег с
/// «вызовным» словом, значение ключа `name`, спецтокен, тег вызова.
static CALL_CONTEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:<[^<>\n]*(?:tool|call|function|invoke|param|arg|name)[^<>\n]*|"(?:name|tool|tool_name|function|recipient_name)"\s*:\s*"|(?:｜\s*>?|\|>)\s*(?:functions\.)?|<[\w:｜| -]*(?:tool|call|function|invoke)[\w:｜| -]*>\s*)$"#,
    )
    .expect("hardcoded regex is valid")
});

/// Страховка: имя инструмента из каталога внутри разметки вызова, которую
/// никто не разобрал. Возвращает кусок текста для диагностики.
pub(super) fn residue(visible: &str, ctx: &ParseCtx) -> Option<String> {
    let mentions = ctx.catalog.mentions()?;
    mentions.find_iter(visible).find_map(|mention| {
        let line_start = visible[..mention.start()].rfind('\n').map_or(0, |i| i + 1);
        // Внутри встроенного кода — упоминание, а не вызов.
        if visible[line_start..mention.start()].matches('`').count() % 2 == 1 {
            return None;
        }
        let from = char_floor(visible, mention.start().saturating_sub(120));
        if !CALL_CONTEXT_RE.is_match(&visible[from..mention.start()]) {
            return None;
        }
        let to = char_floor(visible, (mention.end() + 120).min(visible.len()));
        Some(visible[from..to].trim().to_string())
    })
}

fn char_floor(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::super::{ParseCtx, ParsedReply, ToolCatalog, parse_text};
    use serde_json::json;

    fn catalog() -> ToolCatalog {
        ToolCatalog::new([
            (
                "read_file".to_string(),
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            ),
            ("bash".to_string(), json!({"type": "object"})),
        ])
    }

    fn parse(text: &str, unattended: bool) -> ParsedReply {
        let catalog = catalog();
        parse_text(
            text,
            &ParseCtx {
                catalog: &catalog,
                tool_outputs: Vec::new(),
                unattended,
            },
        )
    }

    #[test]
    fn a_renamed_tool_runs_only_through_a_person() {
        let text =
            "<tool_use><name>Read-File</name><arguments>{\"path\": \"a\"}</arguments></tool_use>";
        let reply = parse(text, false);
        assert_eq!(reply.calls[0].name, "read_file");
        assert_eq!(
            reply.calls[0].origin.written_name.as_deref(),
            Some("Read-File")
        );
        assert!(reply.calls[0].origin.needs_person());

        let reply = parse(text, true);
        assert!(reply.calls.is_empty());
        assert!(
            reply.errors[0].contains("did you mean `read_file`"),
            "{:?}",
            reply.errors
        );
    }

    #[test]
    fn our_own_and_deepseek_formats_run_without_a_person() {
        let reply = parse(
            "<｜DSML｜invoke name=\"bash\"><｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter></｜DSML｜invoke>",
            true,
        );
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert!(!reply.calls[0].origin.needs_person());
    }

    /// Единственный параметр `arguments`, которого нет в схеме, — обёртка.
    #[test]
    fn a_wrapper_argument_unwraps_by_the_schema() {
        let text = "<｜DSML｜invoke name=\"read_file\"><｜DSML｜parameter name=\"arguments\" string=\"false\">{\"path\": \"a.rs\"}</｜DSML｜parameter></｜DSML｜invoke>";
        let reply = parse(text, false);
        assert_eq!(reply.calls[0].arguments, json!({"path": "a.rs"}));
    }

    #[test]
    fn residue_catches_markup_around_a_known_tool_but_not_prose() {
        for markup in [
            "Reading it: <run_tool:read_file path=\"a\"/>",
            "{\"tool\": \"read_file\", \"input\": {\"path\": \"a\"}}",
            "<my_call>read_file\n<k>path</k><v>a</v></my_call>",
        ] {
            let suspect = parse(markup, false).suspect;
            assert!(suspect.is_some_and(|s| s.contains("read_file")), "{markup}");
        }
        for prose in [
            "I'll use read_file on it next.",
            "Wrap it like `<x read_file>` there.",
            "<ToolTip>Click to edit</ToolTip> and bash on.",
            "| read_file | variant |\n|---|---|",
            "{\"name\": \"my-app\", \"scripts\": {\"bash\": \"x\"}}",
        ] {
            assert!(parse(prose, false).suspect.is_none(), "{prose}");
        }
    }
}
