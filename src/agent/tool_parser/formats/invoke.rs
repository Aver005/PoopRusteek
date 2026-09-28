//! Семейство invoke/parameter: DSML (родной формат DeepSeek V3.2–V4.1) и
//! форматы того же устройства с другой приставкой тегов.

use super::{Declared, Family, Found, Param, RawArgs, RawCall, json_at, skip_ws};
use regex::{Captures, Match, Regex};
use std::sync::LazyLock;

/// Как обрезать строковое значение — так, как это делает эталон формата.
#[derive(Clone, Copy)]
enum Trim {
    None,
    /// Ровно один перевод строки с каждого края.
    OneNewline,
    All,
}

/// Диалект: приставка тегов `invoke`/`parameter`, обёртки блока целиком (со
/// своей приставкой), смысл значения без атрибута `string` и обрезка.
struct Dialect {
    format: &'static str,
    prefix: &'static str,
    blocks: String,
    untyped: Declared,
    trim: Trim,
}

/// DSML. Веб отдаёт маркер с удвоенной чертой (`<｜｜DSML｜｜ calls>`), у V4.1
/// перед именем тега пробел — терпим любое число черт и пробелов.
const DSML: &str = r"[｜|]+\s*DSML\s*[｜|]+\s*";
const GCML: &str = r"[｜|]+\s*GCML\s*[｜|]+\s*";

fn dialects() -> [Dialect; 4] {
    [
        // Без атрибута `string` — как `"false"` (vLLM); строки не обрезаются.
        Dialect {
            format: "dsml",
            prefix: DSML,
            blocks: format!(r"{DSML}(?:(?:\w+_)?calls|toolcalls|tool)"),
            untyped: Declared::Json,
            trim: Trim::None,
        },
        // Claude-подобный `<function_calls>`, MiniMax M2 `<minimax:tool_call>`,
        // dots `<dots_function_call>`: теги invoke/parameter без приставки.
        Dialect {
            format: "invoke",
            prefix: "(?:antml:)?",
            blocks: "(?:antml:)?function_calls|minimax:tool_call|dots_function_call".to_string(),
            untyped: Declared::BySchema,
            trim: Trim::OneNewline,
        },
        // GigaChat 3.5 (sglang `gigachat35_detector.py`).
        Dialect {
            format: "gcml",
            prefix: GCML,
            blocks: format!("{GCML}tool_calls"),
            untyped: Declared::Json,
            trim: Trim::None,
        },
        // Внутренность Step3; снаружи её держат спецтокены (`tokens.rs`).
        Dialect {
            format: "step3",
            prefix: "steptml:",
            blocks: "steptml:function_calls".to_string(),
            untyped: Declared::BySchema,
            trim: Trim::All,
        },
    ]
}

/// Имя в кавычках, в апострофах или без них (dots).
const NAME: &str = r#"name\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>/"']+))"#;

struct Grammar {
    format: &'static str,
    untyped: Declared,
    trim: Trim,
    opener: String,
    block_open: Regex,
    block_close: Regex,
    invoke_open: Regex,
    invoke_close: Regex,
    param_open: Regex,
    param_close: Regex,
    /// Любой тег диалекта: за настоящим закрытием значения идёт он.
    tag: Regex,
}

impl Grammar {
    fn new(dialect: Dialect) -> Self {
        let Dialect {
            format,
            prefix: p,
            blocks: b,
            untyped,
            trim,
        } = dialect;
        let re = |pattern: String| Regex::new(&pattern).expect("dialect regexes are valid");
        Self {
            format,
            untyped,
            trim,
            opener: format!(r"<(?:(?:{b})\s*>|{p}invoke\s+name\s*=)"),
            block_open: re(format!(r"<(?:{b})\s*>")),
            block_close: re(format!(r"</(?:{b})\s*>")),
            invoke_open: re(format!(r"<{p}invoke\s+{NAME}\s*(/?)>")),
            invoke_close: re(format!(r"</{p}invoke\s*>")),
            param_open: re(format!(
                r#"<{p}parameter\s+{NAME}(?:\s+string\s*=\s*"(true|false)")?\s*>"#
            )),
            param_close: re(format!(r"</{p}parameter\s*>")),
            tag: re(format!(r"</?{p}\w")),
        }
    }
}

static GRAMMARS: LazyLock<Vec<Grammar>> =
    LazyLock::new(|| dialects().into_iter().map(Grammar::new).collect());

pub(super) fn families() -> Vec<Family> {
    let parsers: [fn(&str, usize) -> Option<Found>; 4] = [
        |hay, at| parse(&GRAMMARS[0], hay, at),
        |hay, at| parse(&GRAMMARS[1], hay, at),
        |hay, at| parse(&GRAMMARS[2], hay, at),
        |hay, at| parse(&GRAMMARS[3], hay, at),
    ];
    GRAMMARS
        .iter()
        .zip(parsers)
        .map(|(grammar, parse)| Family {
            opener: grammar.opener.clone(),
            parse,
        })
        .collect()
}

/// `invoke` диалекта Step3 с позиции `at` — для спецтокенов Step3.
pub(super) fn steptml_invoke(hay: &str, at: usize) -> Option<Found> {
    parse_invoke(&GRAMMARS[3], hay, at)
}

fn trimmed(text: &str, trim: Trim) -> String {
    match trim {
        Trim::None => text.to_string(),
        Trim::All => text.trim().to_string(),
        Trim::OneNewline => {
            let text = text
                .strip_prefix("\r\n")
                .or_else(|| text.strip_prefix('\n'))
                .unwrap_or(text);
            let text = text
                .strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(text);
            text.to_string()
        }
    }
}

/// Совпадение ровно на позиции `at`.
fn at_pos<'h>(re: &Regex, hay: &'h str, at: usize) -> Option<Match<'h>> {
    re.find_at(hay, at).filter(|m| m.start() == at)
}

fn captures_at_pos<'h>(re: &Regex, hay: &'h str, at: usize) -> Option<Captures<'h>> {
    re.captures_at(hay, at)
        .filter(|caps| caps.get(0).is_some_and(|m| m.start() == at))
}

fn parse(g: &Grammar, hay: &str, at: usize) -> Option<Found> {
    match at_pos(&g.block_open, hay, at) {
        Some(open) => parse_block(g, hay, at, open.end()),
        None => parse_invoke(g, hay, at),
    }
}

/// Обёртка с одним или несколькими `invoke`. Без закрытия обёртки берём
/// целые `invoke` до конца текста.
fn parse_block(g: &Grammar, hay: &str, at: usize, from: usize) -> Option<Found> {
    // Процитированная обёртка без вызова следом — проза.
    at_pos(&g.invoke_open, hay, skip_ws(hay, from))?;
    let mut items = Vec::new();
    let mut pos = from;
    loop {
        let open = g.invoke_open.find_at(hay, pos);
        let open = match (open, g.block_close.find_at(hay, pos)) {
            (Some(open), Some(close)) if open.start() < close.start() => open,
            (Some(open), None) => open,
            (_, Some(close)) => {
                return (!items.is_empty()).then(|| Found {
                    span: at..close.end(),
                    items,
                });
            }
            (None, None) => break,
        };
        let Some(found) = parse_invoke(g, hay, open.start()) else {
            pos = open.end();
            continue;
        };
        pos = found.span.end;
        let broken = found.items.iter().any(Result::is_err);
        items.extend(found.items);
        if broken {
            return Some(Found {
                span: at..hay.len(),
                items,
            });
        }
    }
    (!items.is_empty()).then_some(Found {
        span: at..pos,
        items,
    })
}

/// Одиночный `invoke`, в обёртке или без неё (sglang #40236).
fn parse_invoke(g: &Grammar, hay: &str, at: usize) -> Option<Found> {
    let open = captures_at_pos(&g.invoke_open, hay, at)?;
    let open_end = open.get(0).expect("group 0").end();
    let name = first_group(&open, 1..=3).trim().to_string();
    let call = |span, args| {
        Found::call(
            span,
            RawCall {
                name: name.clone(),
                args,
                format: g.format,
            },
        )
    };
    // Вызов без аргументов V4 пишет самозакрытым тегом.
    if open.get(4).is_some_and(|slash| !slash.is_empty()) {
        return Some(call(at..open_end, RawArgs::Params(Vec::new())));
    }
    let body = skip_ws(hay, open_end);
    if hay[body..].starts_with('{') {
        return Some(json_body(g, hay, at, body, &name));
    }
    let mut params = Vec::new();
    let mut pos = open_end;
    loop {
        let p = skip_ws(hay, pos);
        if let Some(close) = at_pos(&g.invoke_close, hay, p) {
            return Some(call(at..close.end(), RawArgs::Params(params)));
        }
        if let Some(param) = captures_at_pos(&g.param_open, hay, p) {
            let value_from = param.get(0).expect("group 0").end();
            let Some((value_end, next)) = value_end(g, hay, value_from) else {
                return Some(Found::broken(at..hay.len(), cut_off(&name)));
            };
            let declared = match param.get(4).map(|m| m.as_str()) {
                Some("true") => Declared::Text,
                Some(_) => Declared::Json,
                None => g.untyped,
            };
            let raw = &hay[value_from..value_end];
            params.push(Param {
                name: first_group(&param, 1..=3).trim().to_string(),
                text: match declared {
                    Declared::Json => raw.to_string(),
                    Declared::Text | Declared::BySchema => trimmed(raw, g.trim),
                },
                declared,
            });
            pos = next;
            continue;
        }
        let next_tag = [g.param_open.find_at(hay, p), g.invoke_close.find_at(hay, p)]
            .into_iter()
            .flatten()
            .map(|m| m.start())
            .min();
        let Some(next_tag) = next_tag else {
            // Обрыв — только если за открытым `invoke` уже пошли параметры;
            // просто процитированный тег — проза.
            return (!params.is_empty()).then(|| Found::broken(at..hay.len(), cut_off(&name)));
        };
        // Обломок тега между параметрами — недобор аргумента, а вызов без
        // аргумента хуже, чем никакой.
        if hay[p..next_tag].contains('<') {
            return Some(Found::broken(
                at..hay.len(),
                format!("tool `{name}`: a malformed <parameter> tag inside the call. Re-send it."),
            ));
        }
        pos = next_tag;
    }
}

/// Тело `invoke` сразу JSON-объектом (V3.2/V4, «Format 2»).
fn json_body(g: &Grammar, hay: &str, at: usize, body: usize, name: &str) -> Found {
    let parsed = json_at(hay, body)
        .ok()
        .filter(|(value, _)| value.is_object())
        .and_then(|(value, end)| Some((value, at_pos(&g.invoke_close, hay, skip_ws(hay, end))?)));
    match parsed {
        Some((value, close)) => Found::call(
            at..close.end(),
            RawCall {
                name: name.to_string(),
                args: RawArgs::Json(value),
                format: g.format,
            },
        ),
        None => Found::broken(
            at..g
                .invoke_close
                .find_at(hay, body)
                .map_or(hay.len(), |c| c.end()),
            format!(
                "tool `{name}`: the <invoke> body is not a valid JSON object. Re-send the call as \
                 <tool_use><name>{name}</name><arguments>{{ valid JSON }}</arguments></tool_use>."
            ),
        ),
    }
}

/// Конец значения и позиция за ним. Закрывающий тег засчитывается, только если
/// за ним тег этого диалекта или конец текста: внутри значения (файл о DSML)
/// он — текст. Незакрытый параметр кончается на следующем, как у sglang.
fn value_end(g: &Grammar, hay: &str, from: usize) -> Option<(usize, usize)> {
    let next_open = g.param_open.find_at(hay, from).map(|m| m.start());
    for close in g.param_close.find_iter(&hay[from..]) {
        let (start, end) = (from + close.start(), from + close.end());
        if next_open.is_some_and(|open| start > open) {
            break;
        }
        let after = skip_ws(hay, end);
        if after == hay.len() || at_pos(&g.tag, hay, after).is_some() {
            return Some((start, end));
        }
    }
    [
        next_open,
        g.invoke_close.find_at(hay, from).map(|m| m.start()),
    ]
    .into_iter()
    .flatten()
    .min()
    .map(|at| (at, at))
}

fn first_group(caps: &Captures, groups: std::ops::RangeInclusive<usize>) -> String {
    groups
        .filter_map(|i| caps.get(i))
        .map(|m| m.as_str().to_string())
        .next()
        .unwrap_or_default()
}

fn cut_off(name: &str) -> String {
    format!(
        "tool `{name}`: an <invoke> block was cut off before its closing tag. Re-send \
         the call as <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
    )
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::parse_reply;
    use serde_json::json;

    /// V4.1 — формат веба: ` calls`, ` invoke`, ` parameter` с ведущим
    /// пробелом (sglang `deepseekv41_detector.py`), блок отделён `\n\n`.
    #[test]
    fn v4_1_block_parses_and_leaves_one_blank_line() {
        let text = "Checking the weather.\n\n<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"get_weather\">\n<｜DSML｜ parameter name=\"city\" string=\"true\">Paris</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>\n\nThen I'll answer.";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
        assert_eq!(reply.visible, "Checking the weather.\n\nThen I'll answer.");
    }

    /// V4: обёртка `tool_calls`, тело JSON-ом (sglang `deepseekv4_detector.py`).
    #[test]
    fn v4_json_body_parses() {
        let text = "<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"get_favorite_tourist_spot\">\n{\"city\": \"San Francisco\"}\n</｜DSML｜invoke>\n</｜DSML｜tool_calls>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "San Francisco");
    }

    /// Вызов без аргументов V4 пишет самозакрытым тегом; раньше он терялся.
    #[test]
    fn a_self_closing_invoke_is_a_call_without_arguments() {
        let text = "<｜DSML｜tool_calls><｜DSML｜invoke name=\"list_tasks\"/><｜DSML｜invoke name=\"x\" /></｜DSML｜tool_calls>";
        let reply = parse_reply(text);
        let names: Vec<_> = reply.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["list_tasks", "x"]);
        assert_eq!(reply.calls[0].arguments, json!({}));
    }

    /// Варианты обёртки из vLLM («observed in production»).
    #[test]
    fn production_wrapper_spellings_parse() {
        for block in ["toolcalls", "tool"] {
            let text = format!(
                "<｜DSML｜{block}><｜DSML｜invoke name=\"a\"></｜DSML｜invoke></｜DSML｜{block}>"
            );
            let reply = parse_reply(&text);
            assert_eq!(reply.calls.len(), 1, "{block}");
            assert_eq!(reply.visible, "", "{block}");
        }
    }

    /// Закрывающий тег внутри значения (файл о DSML) — текст, а не конец.
    #[test]
    fn a_closing_tag_quoted_inside_a_value_keeps_the_whole_value() {
        let text = "<｜DSML｜invoke name=\"write\"><｜DSML｜parameter name=\"content\" string=\"true\">ends with </｜DSML｜parameter> here</｜DSML｜parameter><｜DSML｜parameter name=\"path\" string=\"true\">a.md</｜DSML｜parameter></｜DSML｜invoke>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments["content"],
            "ends with </｜DSML｜parameter> here"
        );
        assert_eq!(reply.calls[0].arguments["path"], "a.md");
    }

    #[test]
    fn an_unclosed_parameter_ends_at_the_next_one() {
        let text = "<｜DSML｜invoke name=\"x\"><｜DSML｜parameter name=\"a\" string=\"true\">1<｜DSML｜parameter name=\"b\" string=\"true\">2</｜DSML｜parameter></｜DSML｜invoke>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls[0].arguments, json!({"a": "1", "b": "2"}));
    }

    /// Обломок тега — недобор аргумента: диагностика, а не вызов без него.
    #[test]
    fn a_malformed_parameter_tag_is_reported_not_dropped() {
        let text = "<｜DSML｜invoke name=\"x\"><｜DSML｜parameter name=\"a\" string=\"true\">1</｜DSML｜parameter><｜DSML｜param name=\"b\">2</｜DSML｜invoke>";
        let reply = parse_reply(text);
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn a_non_object_json_body_is_reported() {
        let reply = parse_reply("<｜DSML｜invoke name=\"x\">{\"a\": }</｜DSML｜invoke>");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    fn weather() -> Vec<(&'static str, serde_json::Value)> {
        vec![(
            "get_weather",
            json!({"type": "object", "properties": {
                "city": {"type": "string"}, "days": {"type": "integer"}
            }}),
        )]
    }

    /// Claude-подобный формат: значения без типа приводятся по схеме, вызов
    /// чужой разметки идёт через человека.
    #[test]
    fn a_claude_like_block_parses_with_schema_types() {
        let text = "<function_calls>\n<invoke name=\"get_weather\">\n<parameter name=\"city\">Paris</parameter>\n<parameter name=\"days\">3</parameter>\n</invoke>\n</function_calls>";
        let reply = crate::agent::tool_parser::parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "Paris", "days": 3})
        );
        assert!(reply.calls[0].origin.needs_person());
        assert_eq!(reply.visible, "");
    }

    /// Дословно из sglang `minimax_m2.py`.
    #[test]
    fn a_minimax_m2_block_parses() {
        let text = "<minimax:tool_call>\n<invoke name=\"func1\">\n<parameter name=\"param1\">value1</parameter>\n<parameter name=\"param2\">value2</parameter>\n</invoke>\n</minimax:tool_call>";
        let reply = crate::agent::tool_parser::parse_with(text, &[("func1", json!({}))]);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"param1": "value1", "param2": "value2"})
        );
    }

    /// dots пишет имя без кавычек.
    #[test]
    fn a_dots_block_with_a_bare_name_parses() {
        let text = "<dots_function_call><invoke name=search><parameter name=\"q\">rust</parameter></invoke></dots_function_call>";
        let reply = crate::agent::tool_parser::parse_with(text, &[("search", json!({}))]);
        assert_eq!(reply.calls[0].arguments, json!({"q": "rust"}));
    }

    #[test]
    fn a_gigachat_gcml_block_parses() {
        let text = "<｜GCML｜tool_calls><｜GCML｜invoke name=\"get_weather\"><｜GCML｜parameter name=\"city\" string=\"true\">Paris</｜GCML｜parameter></｜GCML｜invoke></｜GCML｜tool_calls>";
        let reply = crate::agent::tool_parser::parse_with(text, &weather());
        assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
    }

    /// Дословно из sglang `step3_detector.py`.
    #[test]
    fn a_step3_call_parses_inside_its_tokens() {
        let text = "<｜tool_calls_begin｜>\n<｜tool_call_begin｜>function<｜tool_sep｜><steptml:invoke name=\"get_weather\">\n<steptml:parameter name=\"city\">Paris</steptml:parameter>\n<steptml:parameter name=\"days\">2</steptml:parameter>\n</steptml:invoke><｜tool_call_end｜>\n<｜tool_calls_end｜>";
        let reply = crate::agent::tool_parser::parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "Paris", "days": 2})
        );
        assert_eq!(reply.visible, "");
    }

    /// Чужая разметка с незнакомым именем — чей-то пример, он остаётся текстом.
    #[test]
    fn a_foreign_call_to_an_unknown_tool_stays_prose() {
        let text = "Example: <function_calls><invoke name=\"nope\"><parameter name=\"a\">1</parameter></invoke></function_calls>";
        let reply = crate::agent::tool_parser::parse_with(text, &weather());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }
}
