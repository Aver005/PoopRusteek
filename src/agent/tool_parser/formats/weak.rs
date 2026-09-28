//! Форматы без родной разметки: списки Python-вызовов (Llama, LFM2, Olmo 3) и
//! голый JSON вызова (xLAM, Llama 3 JSON, Phi-4). Источники: sglang
//! `pythonic_detector.py`/`lfm2_detector.py`, vLLM `pythonic_tool_parser.py`,
//! `llama4_pythonic_tool_parser.py`, `lfm2_tool_parser.py`, `olmo3_tool_parser.py`.

use super::{Family, Found, RawArgs, RawCall, skip_ws};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Литералы Python: строки, числа, `True`/`False`/`None`, списки, кортежи,
// словари со строковыми ключами. Никаких выражений и переменных.
// ---------------------------------------------------------------------------

fn ident(src: &str, pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let mut i = pos;
    if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return None;
    }
    i += 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    Some((&src[pos..i], i))
}

fn number_literal(src: &str, pos: usize) -> Result<(Value, usize), String> {
    let bytes = src.as_bytes();
    let mut i = pos;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return Err("expected a number literal".into());
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return Err("expected digits after the decimal point".into());
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            is_float = true;
            i = j;
        }
    }
    let text = &src[pos..i];
    if is_float {
        let f: f64 = text
            .parse()
            .map_err(|_| format!("invalid number literal `{text}`"))?;
        let number = serde_json::Number::from_f64(f)
            .ok_or_else(|| format!("invalid number literal `{text}`"))?;
        Ok((Value::Number(number), i))
    } else {
        let n: i64 = text
            .parse()
            .map_err(|_| format!("invalid number literal `{text}`"))?;
        Ok((Value::Number(n.into()), i))
    }
}

/// Строка в `'…'`/`"…"`. Незнакомое экранирование Python оставляет как есть:
/// обратный слэш и символ.
fn string_literal(src: &str, pos: usize) -> Result<(String, usize), String> {
    let quote = src[pos..]
        .chars()
        .next()
        .expect("caller checked the quote char");
    let body_start = pos + quote.len_utf8();
    let mut out = String::new();
    let mut chars = src[body_start..].char_indices();
    while let Some((idx, ch)) = chars.next() {
        if ch == quote {
            return Ok((out, body_start + idx + ch.len_utf8()));
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some((_, escaped)) = chars.next() else {
            return Err("string literal cut off after a backslash".into());
        };
        match escaped {
            '\\' | '\'' | '"' => out.push(escaped),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '0' => out.push('\0'),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Err("unterminated string literal".into())
}

fn list_literal(src: &str, pos: usize, json_names: bool) -> Result<(Value, usize), String> {
    let mut pos = skip_ws(src, pos + 1);
    let mut items = Vec::new();
    if src[pos..].starts_with(']') {
        return Ok((Value::Array(items), pos + 1));
    }
    loop {
        let (v, after) = value(src, pos, json_names)?;
        items.push(v);
        pos = skip_ws(src, after);
        if src[pos..].starts_with(',') {
            pos = skip_ws(src, pos + 1);
            if src[pos..].starts_with(']') {
                break;
            }
            continue;
        }
        break;
    }
    if !src[pos..].starts_with(']') {
        return Err("missing closing `]`".into());
    }
    Ok((Value::Array(items), pos + 1))
}

/// Кортеж `(…)` — как массив: у JSON нет своего типа для кортежей.
fn tuple_literal(src: &str, pos: usize, json_names: bool) -> Result<(Value, usize), String> {
    let mut pos = skip_ws(src, pos + 1);
    let mut items = Vec::new();
    if src[pos..].starts_with(')') {
        return Ok((Value::Array(items), pos + 1));
    }
    loop {
        let (v, after) = value(src, pos, json_names)?;
        items.push(v);
        pos = skip_ws(src, after);
        if src[pos..].starts_with(',') {
            pos = skip_ws(src, pos + 1);
            if src[pos..].starts_with(')') {
                break;
            }
            continue;
        }
        break;
    }
    if !src[pos..].starts_with(')') {
        return Err("missing closing `)`".into());
    }
    Ok((Value::Array(items), pos + 1))
}

fn dict_literal(src: &str, pos: usize, json_names: bool) -> Result<(Value, usize), String> {
    let mut pos = skip_ws(src, pos + 1);
    let mut map = serde_json::Map::new();
    if src[pos..].starts_with('}') {
        return Ok((Value::Object(map), pos + 1));
    }
    loop {
        pos = skip_ws(src, pos);
        if !matches!(src[pos..].chars().next(), Some('\'') | Some('"')) {
            return Err("dict keys must be string literals".into());
        }
        let (key, after_key) = string_literal(src, pos)?;
        pos = skip_ws(src, after_key);
        if !src[pos..].starts_with(':') {
            return Err("expected `:` after a dict key".into());
        }
        pos = skip_ws(src, pos + 1);
        let (v, after_v) = value(src, pos, json_names)?;
        map.insert(key, v);
        pos = skip_ws(src, after_v);
        if src[pos..].starts_with(',') {
            pos = skip_ws(src, pos + 1);
            if src[pos..].starts_with('}') {
                break;
            }
            continue;
        }
        break;
    }
    if !src[pos..].starts_with('}') {
        return Err("missing closing `}`".into());
    }
    Ok((Value::Object(map), pos + 1))
}

/// `json_names` — принимать также `null`/`true`/`false` (Olmo 3 путает их с
/// `None`/`True`/`False`), в дополнение к настоящим литералам Python.
fn value(src: &str, pos: usize, json_names: bool) -> Result<(Value, usize), String> {
    let pos = skip_ws(src, pos);
    let Some(c) = src[pos..].chars().next() else {
        return Err("expected a value".into());
    };
    if c == '\'' || c == '"' {
        let (s, end) = string_literal(src, pos)?;
        return Ok((Value::String(s), end));
    }
    if c == '[' {
        return list_literal(src, pos, json_names);
    }
    if c == '(' {
        return tuple_literal(src, pos, json_names);
    }
    if c == '{' {
        return dict_literal(src, pos, json_names);
    }
    if c == '+' || c == '-' || c.is_ascii_digit() {
        return number_literal(src, pos);
    }
    if c == '_' || c.is_ascii_alphabetic() {
        let (name, end) = ident(src, pos).expect("checked the first char is an identifier start");
        return match (name, json_names) {
            ("True", _) => Ok((Value::Bool(true), end)),
            ("False", _) => Ok((Value::Bool(false), end)),
            ("None", _) => Ok((Value::Null, end)),
            ("true", true) => Ok((Value::Bool(true), end)),
            ("false", true) => Ok((Value::Bool(false), end)),
            ("null", true) => Ok((Value::Null, end)),
            (other, _) => Err(format!(
                "`{other}` is not a literal value; variables are not allowed"
            )),
        };
    }
    Err(format!("unexpected character `{c}` in a value"))
}

/// `name(kw=value, …)`. Позиционный аргумент — ошибка: схема есть только для
/// именованных.
fn call(src: &str, pos: usize, json_names: bool) -> Result<(String, Value, usize), String> {
    let pos = skip_ws(src, pos);
    let Some((name, after_name)) = ident(src, pos) else {
        return Err("expected a tool name".into());
    };
    let name = name.to_string();
    let mut pos = skip_ws(src, after_name);
    if !src[pos..].starts_with('(') {
        return Err(format!("tool `{name}`: expected `(` after the name"));
    }
    pos = skip_ws(src, pos + 1);
    let mut args = serde_json::Map::new();
    if !src[pos..].starts_with(')') {
        loop {
            pos = skip_ws(src, pos);
            let Some((kw, after_kw)) = ident(src, pos) else {
                return Err(format!(
                    "tool `{name}`: positional arguments are not allowed, use name=value"
                ));
            };
            let after_eq = skip_ws(src, after_kw);
            if !src[after_eq..].starts_with('=') {
                return Err(format!(
                    "tool `{name}`: positional arguments are not allowed, use name=value"
                ));
            }
            pos = skip_ws(src, after_eq + 1);
            let (v, after_v) = value(src, pos, json_names)?;
            args.insert(kw.to_string(), v);
            pos = skip_ws(src, after_v);
            if src[pos..].starts_with(',') {
                pos = skip_ws(src, pos + 1);
                if src[pos..].starts_with(')') {
                    break;
                }
                continue;
            }
            break;
        }
    }
    if !src[pos..].starts_with(')') {
        return Err(format!("tool `{name}`: missing closing `)`"));
    }
    Ok((name, Value::Object(args), pos + 1))
}

fn call_list(
    src: &str,
    pos: usize,
    json_names: bool,
) -> Result<(Vec<(String, Value)>, usize), String> {
    let mut pos = skip_ws(src, pos + 1);
    let mut calls = Vec::new();
    if src[pos..].starts_with(']') {
        return Ok((calls, pos + 1));
    }
    loop {
        let (name, args, after) = call(src, pos, json_names)?;
        calls.push((name, args));
        pos = skip_ws(src, after);
        if src[pos..].starts_with(',') {
            pos = skip_ws(src, pos + 1);
            if src[pos..].starts_with(']') {
                break;
            }
            continue;
        }
        break;
    }
    if !src[pos..].starts_with(']') {
        return Err("missing closing `]`".into());
    }
    Ok((calls, pos + 1))
}

/// Вызовы один за другим без обёртки, запятой или переводом строки —
/// как пишет Olmo 3.
fn bare_calls(
    src: &str,
    mut pos: usize,
    json_names: bool,
) -> Result<(Vec<(String, Value)>, usize), String> {
    let mut calls = Vec::new();
    loop {
        let (name, args, after) = call(src, pos, json_names)?;
        calls.push((name, args));
        pos = skip_ws(src, after);
        if src[pos..].starts_with(',') {
            pos = skip_ws(src, pos + 1);
            continue;
        }
        match src[pos..].chars().next() {
            Some(c) if c == '_' || c.is_ascii_alphabetic() => continue,
            _ => break,
        }
    }
    Ok((calls, pos))
}

fn parse_call_batch(src: &str, json_names: bool) -> Result<Vec<(String, Value)>, String> {
    let pos = skip_ws(src, 0);
    if pos >= src.len() {
        return Err("no tool calls found".into());
    }
    let (calls, end) = if src[pos..].starts_with('[') {
        call_list(src, pos, json_names)?
    } else {
        bare_calls(src, pos, json_names)?
    };
    let end = skip_ws(src, end);
    if end != src.len() {
        return Err("unexpected text after the tool calls".into());
    }
    if calls.is_empty() {
        return Err("no tool calls found".into());
    }
    Ok(calls)
}

/// Список `[f(a=1), g()]` или вызовы один за другим, целиком. Только
/// литералы Python — см. `value`.
pub(super) fn parse_pythonic(src: &str) -> Result<Vec<(String, Value)>, String> {
    parse_call_batch(src, false)
}

// ---------------------------------------------------------------------------
// Сильные семейства: свой маркер, тело pythonic (или JSON у LFM2).
// ---------------------------------------------------------------------------

const LFM2_OPEN: &str = "<|tool_call_start|>";
const LFM2_CLOSE: &str = "<|tool_call_end|>";
const LFM2_LABEL: &str = "LFM2 <|tool_call_start|>";

const OLMO3_OPEN: &str = "<function_calls>";
const OLMO3_CLOSE: &str = "</function_calls>";
const OLMO3_LABEL: &str = "Olmo 3 <function_calls>";

const PY_OPEN: &str = "<|python_start|>";
const PY_CLOSE: &str = "<|python_end|>";
const PY_LABEL: &str = "Llama 4 <|python_start|>";

pub(super) fn families() -> Vec<Family> {
    vec![
        Family {
            opener: regex::escape(LFM2_OPEN),
            parse: parse_lfm2,
        },
        Family {
            opener: regex::escape(OLMO3_OPEN),
            parse: parse_olmo3,
        },
        Family {
            opener: regex::escape(PY_OPEN),
            parse: parse_llama4,
        },
    ]
}

fn broken_block(marker: &str, why: &str) -> String {
    format!(
        "a {marker} tool-call block could not be parsed: {why}. Re-send the call as \
         <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
    )
}

fn to_raw_calls(calls: Vec<(String, Value)>, format: &'static str) -> Vec<RawCall> {
    calls
        .into_iter()
        .map(|(name, args)| RawCall {
            name,
            args: RawArgs::Json(args),
            format,
        })
        .collect()
}

/// Похоже на начало вызова (список, JSON-объект, голое имя) — а не на хвост
/// прозы после маркера, процитированного мимоходом.
fn looks_like_a_call(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_some_and(|c| c == '[' || c == '{' || c == '_' || c.is_ascii_alphabetic())
}

fn parse_lfm2(hay: &str, at: usize) -> Option<Found> {
    let body_start = skip_ws(hay, at + LFM2_OPEN.len());
    if body_start >= hay.len() || !looks_like_a_call(&hay[body_start..]) {
        return None;
    }
    let Some(offset) = hay[body_start..].find(LFM2_CLOSE) else {
        return Some(Found::broken(
            at..hay.len(),
            broken_block(
                LFM2_LABEL,
                "the block has no closing <|tool_call_end|> token",
            ),
        ));
    };
    let body_end = body_start + offset;
    let span_end = body_end + LFM2_CLOSE.len();
    match lfm2_body(&hay[body_start..body_end]) {
        Ok(calls) => Some(Found {
            span: at..span_end,
            items: calls.into_iter().map(Ok).collect(),
        }),
        Err(reason) => Some(Found::broken(
            at..span_end,
            broken_block(LFM2_LABEL, &reason),
        )),
    }
}

/// LFM2 принимает pythonic-тело или JSON: JSON-форму проверяем первой, как
/// у sglang (`content.startswith("[{") or content.startswith("{")`).
fn lfm2_body(body: &str) -> Result<Vec<RawCall>, String> {
    let body = body.trim();
    if body.starts_with('{') || body.starts_with("[{") {
        return json_calls(body, "lfm2");
    }
    parse_pythonic(body).map(|calls| to_raw_calls(calls, "lfm2"))
}

fn parse_olmo3(hay: &str, at: usize) -> Option<Found> {
    let body_start = skip_ws(hay, at + OLMO3_OPEN.len());
    if body_start >= hay.len() || !looks_like_a_call(&hay[body_start..]) {
        // Пусто, прозаический хвост, или тело в духе Claude/dots
        // (`<invoke ...>`) — не наш формат.
        return None;
    }
    let Some(offset) = hay[body_start..].find(OLMO3_CLOSE) else {
        return Some(Found::broken(
            at..hay.len(),
            broken_block(
                OLMO3_LABEL,
                "the block has no closing </function_calls> tag",
            ),
        ));
    };
    let body_end = body_start + offset;
    let span_end = body_end + OLMO3_CLOSE.len();
    // Olmo 3 путает `null`/`true`/`false` с `None`/`True`/`False` — допускаем оба.
    match parse_call_batch(&hay[body_start..body_end], true) {
        Ok(calls) => Some(Found {
            span: at..span_end,
            items: to_raw_calls(calls, "olmo3").into_iter().map(Ok).collect(),
        }),
        Err(reason) => Some(Found::broken(
            at..span_end,
            broken_block(OLMO3_LABEL, &reason),
        )),
    }
}

fn parse_llama4(hay: &str, at: usize) -> Option<Found> {
    let body_start = skip_ws(hay, at + PY_OPEN.len());
    if body_start >= hay.len() || !looks_like_a_call(&hay[body_start..]) {
        return None;
    }
    let Some(offset) = hay[body_start..].find(PY_CLOSE) else {
        return Some(Found::broken(
            at..hay.len(),
            broken_block(PY_LABEL, "the block has no closing <|python_end|> token"),
        ));
    };
    let body_end = body_start + offset;
    let span_end = body_end + PY_CLOSE.len();
    match parse_pythonic(&hay[body_start..body_end]) {
        Ok(calls) => Some(Found {
            span: at..span_end,
            items: to_raw_calls(calls, "llama4").into_iter().map(Ok).collect(),
        }),
        Err(reason) => Some(Found::broken(at..span_end, broken_block(PY_LABEL, &reason))),
    }
}

// ---------------------------------------------------------------------------
// JSON-вызов без разметки: `{"name":…, "arguments"|"parameters"|"args"|"input":…}`
// или массив таких. Общий помощник для LFM2 и для `whole_reply`.
// ---------------------------------------------------------------------------

fn json_call(value: &Value, format: &'static str) -> Result<RawCall, String> {
    let Some(obj) = value.as_object() else {
        return Err("each call must be a JSON object".into());
    };
    if is_tool_definition(obj) {
        return Err("this is a tool definition, not a call".into());
    }
    let Some(name) = obj.get("name").and_then(Value::as_str) else {
        return Err("a call object has no \"name\" field".into());
    };
    let args = ["arguments", "parameters", "args", "input"]
        .into_iter()
        .find_map(|key| obj.get(key))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    Ok(RawCall {
        name: name.to_string(),
        args: RawArgs::Json(args),
        format,
    })
}

/// Определение инструмента (`description` + `parameters.properties`), не вызов.
fn is_tool_definition(obj: &serde_json::Map<String, Value>) -> bool {
    obj.contains_key("description")
        && obj
            .get("parameters")
            .and_then(Value::as_object)
            .is_some_and(|params| params.contains_key("properties"))
}

fn json_calls(body: &str, format: &'static str) -> Result<Vec<RawCall>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("the JSON is not valid ({error})"))?;
    if value.is_object() {
        return Ok(vec![json_call(&value, format)?]);
    }
    match value {
        Value::Array(items) if !items.is_empty() => {
            items.iter().map(|item| json_call(item, format)).collect()
        }
        Value::Array(_) => Err("the call list is empty".into()),
        _ => Err("expected a JSON object or an array of calls".into()),
    }
}

// ---------------------------------------------------------------------------
// Слабые форматы: разбираются, только если ВЕСЬ ответ — вызовы.
// ---------------------------------------------------------------------------

/// Ровно один блок ```` ```json ````/````` ``` `````/```` ```python ````/
/// ```` ```tool_code ```` вокруг всего текста; иначе `None`.
fn fenced_block(text: &str) -> Option<&str> {
    let after_open = text.strip_prefix("```")?;
    let line_end = after_open.find('\n')?;
    let lang = after_open[..line_end].trim();
    if !lang.is_empty() && !matches!(lang, "json" | "python" | "tool_code") {
        return None;
    }
    let body = &after_open[line_end + 1..];
    let close = body.rfind("```")?;
    if !body[close + 3..].trim().is_empty() {
        return None;
    }
    Some(&body[..close])
}

fn whole_reply_body(body: &str) -> Option<Vec<RawCall>> {
    if let Ok(calls) = parse_pythonic(body) {
        return Some(to_raw_calls(calls, "pythonic"));
    }
    json_calls(body, "json").ok()
}

/// `text`, обрезанный по краям, целиком — список/одиночные pythonic-вызовы,
/// либо голый JSON вызова (объект или массив), либо то же в одном блоке кода.
/// Позиций нет: годится только когда годится всё.
pub(in crate::agent::tool_parser) fn whole_reply(text: &str) -> Option<Vec<RawCall>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    match fenced_block(trimmed) {
        Some(inner) => whole_reply_body(inner.trim()),
        None => whole_reply_body(trimmed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool_parser::parse_with;
    use serde_json::json;

    // ---- parse_pythonic ----

    /// Дословно из vLLM `test_pythonic_tool_parser.py` (SIMPLE_FUNCTION_OUTPUT).
    #[test]
    fn a_single_bare_call_parses() {
        let calls = parse_pythonic("get_weather(city='San Francisco', metric='celsius')").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "get_weather");
        assert_eq!(
            calls[0].1,
            json!({"city": "San Francisco", "metric": "celsius"})
        );
    }

    #[test]
    fn a_bracketed_list_of_calls_parses() {
        let calls = parse_pythonic("[get_weather(city='LA'), get_time(city='LA')]").unwrap();
        let names: Vec<_> = calls.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["get_weather", "get_time"]);
    }

    #[test]
    fn multiple_bare_calls_without_brackets_parse() {
        let calls = parse_pythonic("a(x=1)\nb(y=2)").unwrap();
        let names: Vec<_> = calls.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// Дословно из vLLM `test_pythonic_tool_parser.py` (MORE_TYPES_FUNCTION_OUTPUT):
    /// строка, число, словарь, `None`, `True`, список — всё вложено в один вызов.
    #[test]
    fn nested_literal_types_all_convert() {
        let text = "register_user(name='John Doe', age=37, address={'city': 'San Francisco', 'state': 'CA'}, role=None, passed_test=True, aliases=['John', 'Johnny'])";
        let calls = parse_pythonic(text).unwrap();
        assert_eq!(
            calls[0].1,
            json!({
                "name": "John Doe",
                "age": 37,
                "address": {"city": "San Francisco", "state": "CA"},
                "role": null,
                "passed_test": true,
                "aliases": ["John", "Johnny"]
            })
        );
    }

    /// Дословно из vLLM `test_pythonic_tool_parser.py` (ESCAPED_STRING_FUNCTION_OUTPUT).
    #[test]
    fn escaped_quotes_in_strings_decode() {
        let text = r#"get_weather(city='Martha\'s Vineyard', metric='\"cool units\"')"#;
        let calls = parse_pythonic(text).unwrap();
        assert_eq!(
            calls[0].1,
            json!({"city": "Martha's Vineyard", "metric": "\"cool units\""})
        );
    }

    #[test]
    fn a_tuple_becomes_a_json_array() {
        let calls = parse_pythonic("resize(size=(800, 600))").unwrap();
        assert_eq!(calls[0].1, json!({"size": [800, 600]}));
    }

    #[test]
    fn signed_numbers_parse() {
        let calls = parse_pythonic("move(dx=-5, dy=+2.5)").unwrap();
        assert_eq!(calls[0].1, json!({"dx": -5, "dy": 2.5}));
    }

    #[test]
    fn a_positional_argument_is_an_error() {
        assert!(parse_pythonic("get_weather('Paris')").is_err());
    }

    #[test]
    fn a_bare_variable_value_is_an_error() {
        assert!(parse_pythonic("f(x=some_var)").is_err());
    }

    #[test]
    fn trailing_text_after_the_calls_is_an_error() {
        assert!(parse_pythonic("f() and then some").is_err());
    }

    #[test]
    fn an_empty_list_is_an_error() {
        assert!(parse_pythonic("[]").is_err());
    }

    // ---- families(): LFM2, Olmo 3, Llama 4 ----

    fn weather() -> Vec<(&'static str, Value)> {
        vec![(
            "get_weather",
            json!({"type": "object", "properties": {
                "city": {"type": "string"}, "metric": {"type": "string"}
            }}),
        )]
    }

    /// Дословно из vLLM `test_lfm2_tool_parser.py` (SIMPLE_FUNCTION_OUTPUT).
    #[test]
    fn lfm2_pythonic_body_parses() {
        let text =
            "<|tool_call_start|>[get_candidate_status(candidate_id='12345')]<|tool_call_end|>";
        let reply = parse_with(text, &[("get_candidate_status", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_candidate_status");
        assert_eq!(reply.calls[0].arguments, json!({"candidate_id": "12345"}));
        assert!(reply.calls[0].origin.needs_person());
        assert_eq!(reply.visible, "");
    }

    /// Дословно из sglang `lfm2_detector.py` (докстрока `Lfm2Detector`, JSON-форма).
    #[test]
    fn lfm2_json_body_parses() {
        let text = r#"<|tool_call_start|>[{"name": "calculator", "arguments": {"expression": "5 * 7"}}]<|tool_call_end|>"#;
        let reply = parse_with(text, &[("calculator", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"expression": "5 * 7"}));
    }

    #[test]
    fn lfm2_multiple_calls_in_one_block_parse() {
        let text = "<|tool_call_start|>[a(x=1), b(y=2)]<|tool_call_end|>";
        let reply = parse_with(text, &[("a", json!({})), ("b", json!({}))]);
        let names: Vec<_> = reply.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn lfm2_without_its_closing_token_is_reported() {
        let text = "<|tool_call_start|>[a(x=1)]";
        let reply = parse_with(text, &[("a", json!({}))]);
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn lfm2_marker_quoted_in_prose_is_left_alone() {
        let text = "The format starts with <|tool_call_start|>.";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
    }

    #[test]
    fn lfm2_broken_body_is_reported() {
        let text = "<|tool_call_start|>[not valid<|tool_call_end|>";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    /// Дословно из vLLM `test_olmo3_tool_parser.py` (SIMPLE_FUNCTION_OUTPUT).
    #[test]
    fn olmo3_single_call_parses() {
        let text =
            "<function_calls>get_weather(city='San Francisco', metric='celsius')</function_calls>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "San Francisco", "metric": "celsius"})
        );
    }

    /// Дословно из vLLM `test_olmo3_tool_parser.py`
    /// (MORE_TYPES_FUNCTION_OUTPUT_JSON_LITERALS): `null`/`true` вместо `None`/`True`.
    #[test]
    fn olmo3_accepts_json_style_literals() {
        let text = "<function_calls>register_user(name='John Doe', age=37, address={'city': 'San Francisco', 'state': 'CA'}, role=null, passed_test=true, aliases=['John', 'Johnny'])</function_calls>";
        let reply = parse_with(text, &[("register_user", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["role"], json!(null));
        assert_eq!(reply.calls[0].arguments["passed_test"], json!(true));
    }

    #[test]
    fn olmo3_newline_separated_calls_parse() {
        let text = "<function_calls>\nget_weather(city='Paris')\nget_weather(city='NYC')\n</function_calls>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[1].arguments["city"], "NYC");
    }

    /// `<function_calls>` занят Claude-подобным форматом (`<invoke>`) — Olmo 3 уступает.
    #[test]
    fn olmo3_defers_to_claude_like_markup() {
        let text = "<function_calls><invoke name=\"x\"><parameter name=\"a\">1</parameter></invoke></function_calls>";
        assert!(parse_olmo3(text, 0).is_none());
    }

    #[test]
    fn olmo3_marker_quoted_in_prose_is_left_alone() {
        let text = "Wrap calls in <function_calls>.";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
    }

    #[test]
    fn olmo3_broken_body_is_reported() {
        let text = "<function_calls>not a call</function_calls>";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    /// Дословно из vLLM `test_llama4_pythonic_tool_parser.py` (PYTHON_TAG_FUNCTION_OUTPUT).
    #[test]
    fn llama4_marker_wrapped_call_parses() {
        let text = "<|python_start|>[get_weather(city='LA', metric='C')]<|python_end|>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "LA", "metric": "C"})
        );
        assert_eq!(reply.visible, "");
    }

    /// По образцу vLLM `test_llama4_pythonic_tool_parser.py` (`test_str`): два вызова.
    #[test]
    fn llama4_multiple_calls_parse() {
        let text = "<|python_start|>[get_weather(city='LA', metric='C'),register_user(name='Doe', age=9)]<|python_end|>";
        let reply = parse_with(
            text,
            &[("get_weather", json!({})), ("register_user", json!({}))],
        );
        let names: Vec<_> = reply.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["get_weather", "register_user"]);
    }

    #[test]
    fn llama4_without_its_closing_token_is_reported() {
        let text = "<|python_start|>[get_weather(city='LA')]";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn llama4_marker_quoted_in_prose_is_left_alone() {
        let text = "Some models wrap code in <|python_start|>.";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
    }

    #[test]
    fn llama4_broken_body_is_reported() {
        let text = "<|python_start|>[not valid<|python_end|>";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    // ---- whole_reply ----

    #[test]
    fn whole_reply_accepts_a_bare_pythonic_call() {
        let calls = whole_reply("get_weather(city='Paris')").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].format, "pythonic");
    }

    /// xLAM/Llama 3 JSON/Phi-4: объект вызова без обёртки.
    #[test]
    fn whole_reply_accepts_a_bare_json_object() {
        let calls =
            whole_reply(r#"{"name": "get_weather", "parameters": {"city": "Paris"}}"#).unwrap();
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].format, "json");
        let RawArgs::Json(args) = &calls[0].args else {
            panic!("expected JSON args")
        };
        assert_eq!(args, &json!({"city": "Paris"}));
    }

    #[test]
    fn whole_reply_accepts_a_json_array_of_calls() {
        let calls =
            whole_reply(r#"[{"name": "a", "arguments": {}}, {"name": "b", "args": {"x": 1}}]"#)
                .unwrap();
        let names: Vec<_> = calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn whole_reply_unwraps_a_single_json_fence() {
        let text =
            "```json\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Paris\"}}\n```";
        let calls = whole_reply(text).unwrap();
        assert_eq!(calls[0].name, "get_weather");
    }

    #[test]
    fn whole_reply_unwraps_a_bare_fence_around_pythonic_calls() {
        let text = "```\n[get_weather(city='Paris')]\n```";
        let calls = whole_reply(text).unwrap();
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].format, "pythonic");
    }

    #[test]
    fn whole_reply_unwraps_a_tool_code_fence() {
        let text = "```tool_code\nget_weather(city='Paris')\n```";
        let calls = whole_reply(text).unwrap();
        assert_eq!(calls[0].name, "get_weather");
    }

    #[test]
    fn whole_reply_rejects_a_tool_definition_object() {
        let text = r#"{"name": "get_weather", "description": "Look up weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}"#;
        assert!(whole_reply(text).is_none());
    }

    #[test]
    fn whole_reply_rejects_a_markdown_link() {
        assert!(whole_reply("[Google](https://google.com)").is_none());
    }

    #[test]
    fn whole_reply_rejects_a_prose_list() {
        assert!(whole_reply("[a, b]").is_none());
    }

    #[test]
    fn whole_reply_rejects_a_call_followed_by_prose() {
        assert!(whole_reply("[see(this)] more text").is_none());
    }

    #[test]
    fn whole_reply_rejects_a_json_example_inside_prose() {
        let text = "For example: {\"name\": \"get_weather\", \"arguments\": {}} is the shape.";
        assert!(whole_reply(text).is_none());
    }

    #[test]
    fn whole_reply_rejects_plain_prose() {
        assert!(whole_reply("Let me check the weather for you.").is_none());
    }
}
