//! Gemma 4 (sglang `gemma4_detector.py`; vLLM `gemma4_engine_tool_parser.py`
//! → `vllm/parser/gemma4.py`) и FunctionGemma (vLLM
//! `functiongemma_tool_parser.py`): один вызов на пару токенов открытия и
//! закрытия, своя грамматика значений — общая для обоих, различается только
//! разделитель строк.

use super::{Family, Found, RawArgs, RawCall, skip_ws};
use serde_json::{Map, Value};

/// Разница между диалектами: токены обёртки, разделитель строк значений и
/// то, какие формы имени/аргументов диалект допускает.
struct Dialect {
    format: &'static str,
    label: &'static str,
    open: &'static str,
    close: &'static str,
    string_delim: &'static str,
    /// Имя может начинаться сразу с `:`, без слова `call` (Gemma 4).
    bare_colon: bool,
    /// Аргументы можно открыть `(` вместо `{` (Gemma 4).
    paren_args: bool,
    /// Вызов вовсе без аргументов — имя сразу перед закрывающим токеном (Gemma 4).
    allow_no_args: bool,
    /// Имя — строго `\w+`; иначе форма явно наша, но сломана (FunctionGemma).
    strict_name: bool,
}

const GEMMA4: Dialect = Dialect {
    format: "gemma4",
    label: "Gemma 4",
    open: "<|tool_call>",
    close: "<tool_call|>",
    string_delim: "<|\"|>",
    bare_colon: true,
    paren_args: true,
    allow_no_args: true,
    strict_name: false,
};

const FUNCTION_GEMMA: Dialect = Dialect {
    format: "function_gemma",
    label: "FunctionGemma",
    open: "<start_function_call>",
    close: "<end_function_call>",
    string_delim: "<escape>",
    bare_colon: false,
    paren_args: false,
    allow_no_args: false,
    strict_name: true,
};

pub(super) fn families() -> Vec<Family> {
    vec![
        Family {
            opener: regex::escape(GEMMA4.open),
            parse: |hay, at| parse(&GEMMA4, hay, at),
        },
        Family {
            opener: regex::escape(FUNCTION_GEMMA.open),
            parse: |hay, at| parse(&FUNCTION_GEMMA, hay, at),
        },
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgsOpen {
    Brace,
    Paren,
    /// Закрывающий токен идёт сразу за именем — вызов без аргументов.
    Absent,
}

fn parse(d: &Dialect, hay: &str, at: usize) -> Option<Found> {
    let after = at + d.open.len();
    let name_start = if let Some(rest) = hay[after..].strip_prefix("call:") {
        hay.len() - rest.len()
    } else if d.bare_colon {
        hay.len() - hay[after..].strip_prefix(':')?.len()
    } else {
        return None;
    };
    let brace = hay[name_start..]
        .find('{')
        .map(|i| (name_start + i, ArgsOpen::Brace));
    let paren = d
        .paren_args
        .then(|| {
            hay[name_start..]
                .find('(')
                .map(|i| (name_start + i, ArgsOpen::Paren))
        })
        .flatten();
    let close = hay[name_start..]
        .find(d.close)
        .map(|i| (name_start + i, ArgsOpen::Absent));
    let Some((marker, kind)) = [brace, paren, close]
        .into_iter()
        .flatten()
        .min_by_key(|&(p, _)| p)
    else {
        return Some(Found::broken(at..hay.len(), cut_off(d)));
    };
    let name = hay[name_start..marker].trim().to_string();
    let valid_name = !name.is_empty()
        && (!d.strict_name || name.chars().all(|c| c.is_alphanumeric() || c == '_'));
    if !valid_name {
        return Some(Found::broken(at..hay.len(), bad_name(d, &name)));
    }
    match kind {
        ArgsOpen::Absent => {
            if !d.allow_no_args {
                return Some(Found::broken(at..hay.len(), missing_args(d, &name)));
            }
            let end = marker + d.close.len();
            Some(Found::call(
                at..end,
                RawCall {
                    name,
                    args: RawArgs::Json(Value::Object(Map::new())),
                    format: d.format,
                },
            ))
        }
        ArgsOpen::Brace | ArgsOpen::Paren => {
            let (open_ch, close_ch) = if kind == ArgsOpen::Brace {
                ('{', '}')
            } else {
                ('(', ')')
            };
            let body_start = marker + open_ch.len_utf8();
            let Some(body_end) = matching_close(hay, body_start, open_ch, close_ch, d.string_delim)
            else {
                return Some(Found::broken(at..hay.len(), cut_off(d)));
            };
            let after_body = skip_ws(hay, body_end + close_ch.len_utf8());
            if !hay[after_body..].starts_with(d.close) {
                return Some(Found::broken(at..hay.len(), trailing(d, &name)));
            }
            let end = after_body + d.close.len();
            let value = parse_object(&hay[body_start..body_end], d.string_delim);
            Some(Found::call(
                at..end,
                RawCall {
                    name,
                    args: RawArgs::Json(value),
                    format: d.format,
                },
            ))
        }
    }
}

/// Конец `{…}`/`(…)`: считаем вложенность сами, область в разделителе строк
/// пропускаем целиком — скобка внутри значения не в счёт.
fn matching_close(text: &str, from: usize, open: char, close: char, delim: &str) -> Option<usize> {
    let mut depth = 1u32;
    let mut i = from;
    loop {
        if text[i..].starts_with(delim) {
            let rest = i + delim.len();
            i = rest + text[rest..].find(delim)? + delim.len();
            continue;
        }
        let off = text[i..].find([open, close])?;
        let ch_pos = i + off;
        let ch = text[ch_pos..].chars().next().expect("find matched a char");
        i = ch_pos + ch.len_utf8();
        if ch == open {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(ch_pos);
            }
        }
    }
}

/// Разбор `key:value,...` без кавычек у ключей (Gemma 4/FunctionGemma).
fn parse_object(text: &str, delim: &str) -> Value {
    let mut map = Map::new();
    let mut i = 0;
    loop {
        i = skip_seps(text, i);
        if i >= text.len() {
            break;
        }
        let Some(colon_rel) = text[i..].find(':') else {
            break;
        };
        let key = text[i..i + colon_rel].trim().to_string();
        i = skip_inline_ws(text, i + colon_rel + 1);
        if i >= text.len() {
            map.insert(key, Value::String(String::new()));
            break;
        }
        let (value, next) = parse_value(text, i, delim);
        map.insert(key, value);
        i = next;
    }
    Value::Object(map)
}

fn parse_array(text: &str, delim: &str) -> Value {
    let mut items = Vec::new();
    let mut i = 0;
    loop {
        i = skip_seps(text, i);
        if i >= text.len() {
            break;
        }
        let (value, next) = parse_value(text, i, delim);
        items.push(value);
        i = next;
    }
    Value::Array(items)
}

/// Одно значение с позиции `i`: строка в `delim`, вложенный объект/массив
/// или голое число/`true`/`false`/`null`/текст. Возвращает конец за ним.
fn parse_value(text: &str, i: usize, delim: &str) -> (Value, usize) {
    if let Some(rest) = text[i..].strip_prefix(delim) {
        let val_start = i + delim.len();
        return match rest.find(delim) {
            Some(rel) => (
                Value::String(text[val_start..val_start + rel].to_string()),
                val_start + rel + delim.len(),
            ),
            None => (Value::String(rest.to_string()), text.len()),
        };
    }
    if text[i..].starts_with('{') {
        let body_start = i + 1;
        return match matching_close(text, body_start, '{', '}', delim) {
            Some(end) => (parse_object(&text[body_start..end], delim), end + 1),
            None => (parse_object(&text[body_start..], delim), text.len()),
        };
    }
    if text[i..].starts_with('[') {
        let body_start = i + 1;
        return match matching_close(text, body_start, '[', ']', delim) {
            Some(end) => (parse_array(&text[body_start..end], delim), end + 1),
            None => (parse_array(&text[body_start..], delim), text.len()),
        };
    }
    let end = text[i..]
        .find([',', '}', ']'])
        .map_or(text.len(), |off| i + off);
    (parse_bare(text[i..end].trim()), end)
}

fn skip_seps(text: &str, pos: usize) -> usize {
    text[pos..]
        .find(|c: char| !matches!(c, ' ' | ',' | '\n' | '\t'))
        .map_or(text.len(), |off| pos + off)
}

fn skip_inline_ws(text: &str, pos: usize) -> usize {
    text[pos..]
        .find(|c: char| !matches!(c, ' ' | '\n' | '\t'))
        .map_or(text.len(), |off| pos + off)
}

/// Голое значение вне разделителя строк: `true`/`false`/`null`, число (`.` —
/// признак float, как у sglang `_parse_gemma4_value`), иначе — текст как есть.
fn parse_bare(text: &str) -> Value {
    match text {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }
    if text.contains('.') {
        if let Ok(f) = text.parse::<f64>()
            && let Some(n) = serde_json::Number::from_f64(f)
        {
            return Value::Number(n);
        }
    } else if let Ok(n) = text.parse::<i64>() {
        return Value::Number(n.into());
    }
    Value::String(text.to_string())
}

fn cut_off(d: &Dialect) -> String {
    format!(
        "a {} tool call was cut off before its closing `{}`. Re-send the call as \
         <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>.",
        d.label, d.close
    )
}

fn bad_name(d: &Dialect, name: &str) -> String {
    format!(
        "a {} tool call has no valid function name (got `{name}`). Re-send the call as \
         <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>.",
        d.label
    )
}

fn missing_args(d: &Dialect, name: &str) -> String {
    format!(
        "tool `{name}`: a {} call has no arguments block. Re-send the call as \
         <tool_use><name>{name}</name><arguments>{{ valid JSON }}</arguments></tool_use>.",
        d.label
    )
}

fn trailing(d: &Dialect, name: &str) -> String {
    format!(
        "tool `{name}`: unexpected text after the {} call arguments. Re-send the call as \
         <tool_use><name>{name}</name><arguments>{{ valid JSON }}</arguments></tool_use>.",
        d.label
    )
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::parse_with;
    use serde_json::json;

    fn tools() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("func_name", json!({"type": "object"})),
            ("get_weather", json!({"type": "object"})),
            ("bash", json!({"type": "object"})),
        ]
    }

    /// Дословно из докстроки vLLM `vllm/parser/gemma4.py` (модуль-уровня).
    #[test]
    fn the_module_docstring_example_parses() {
        let text = "<|tool_call>call:func_name{key:<|\"|>value<|\"|>,num:42}<tool_call|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "func_name");
        assert_eq!(reply.calls[0].arguments, json!({"key": "value", "num": 42}));
        assert_eq!(reply.visible, "");
    }

    /// Вариант без слова `call`: `<|tool_call>:имя{...}` (sglang
    /// `gemma4_config` — переход `TOOL_PREAMBLE` -> `TOOL_NAME` по `:`).
    #[test]
    fn the_bare_colon_opener_parses() {
        let text = "<|tool_call>:get_weather{city:<|\"|>Rome<|\"|>}<tool_call|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].arguments, json!({"city": "Rome"}));
    }

    /// Вариант с круглыми скобками: `call:имя(...)` вместо `call:имя{...}`.
    #[test]
    fn the_paren_delimited_arguments_parse() {
        let text = "<|tool_call>call:bash(command:<|\"|>ls<|\"|>)<tool_call|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"command": "ls"}));
    }

    /// Вызов вовсе без аргументов: имя сразу перед закрывающим токеном.
    #[test]
    fn a_call_with_no_arguments_block_parses() {
        let text = "<|tool_call>call:bash<tool_call|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({}));
    }

    /// Вложенные объект и массив, число с точкой, булево и `null` — как в
    /// докстроке sglang `gemma4_detector.py` `k:<|"|>v<|"|>,n:42,o:{…},a:[…]`.
    #[test]
    fn nested_objects_arrays_and_bare_types_parse() {
        let text = "<|tool_call>call:get_weather{k:<|\"|>v<|\"|>,n:42,pi:3.5,flag:true,missing:null,o:{inner:<|\"|>x<|\"|>},a:[1,<|\"|>b<|\"|>,{c:2}]}<tool_call|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({
                "k": "v", "n": 42, "pi": 3.5, "flag": true, "missing": null,
                "o": {"inner": "x"}, "a": [1, "b", {"c": 2}]
            })
        );
    }

    #[test]
    fn a_marker_quoted_in_prose_is_not_a_call() {
        let text = "Gemma 4 writes tool calls with a <|tool_call> token.";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    #[test]
    fn a_call_cut_off_before_its_closing_token_is_reported() {
        let text = "<|tool_call>call:bash{command:<|\"|>ls";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Дословно из докстроки класса vLLM `functiongemma_tool_parser.py`.
    #[test]
    fn the_class_docstring_example_parses() {
        let text =
            "<start_function_call>call:func_name{param:<escape>value<escape>}<end_function_call>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "func_name");
        assert_eq!(reply.calls[0].arguments, json!({"param": "value"}));
        assert_eq!(reply.visible, "");
    }

    /// Несколько аргументов, тот же разделитель `<escape>`.
    #[test]
    fn function_gemma_multiple_arguments_parse() {
        let text = "<start_function_call>call:get_weather{city:<escape>Paris<escape>,days:<escape>3<escape>}<end_function_call>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "Paris", "days": "3"})
        );
    }

    #[test]
    fn function_gemma_marker_quoted_in_prose_is_not_a_call() {
        let text = "FunctionGemma opens with <start_function_call> before a call.";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    #[test]
    fn function_gemma_cut_off_call_is_reported_not_dropped() {
        let text = "<start_function_call>call:bash{command:<escape>ls";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// FunctionGemma не допускает `call:` без слова `call`, и её имя — строго
    /// `\w+`: небуквенно-цифровой хвост делает вызов сломанным, а не прозой.
    #[test]
    fn function_gemma_requires_the_call_prefix() {
        let text = "<start_function_call>:bash{command:<escape>ls<escape>}<end_function_call>";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }
}
