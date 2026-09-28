//! Kimi K2 (sglang `kimik2_detector.py`; vLLM `kimi_k2_tool_parser.py` →
//! `vllm/parser/kimi_k2.py`) и Kimi K3 (sglang `kimik3_detector.py`,
//! `kimik3_format.py`; vLLM `kimi_k3_tool_parser.py`) — два разных формата
//! под общим именем модели.

use super::{Declared, Family, Found, Param, RawArgs, RawCall, json_at, skip_ws};
use regex::Regex;
use std::sync::LazyLock;

// --- Kimi K2: <|tool_calls_section_begin|>...<|tool_call_begin|>functions.name:0<|tool_call_argument_begin|>{json}<|tool_call_end|>...<|tool_calls_section_end|>

const K2_SECTION_OPEN: &str = "<|tool_calls_section_begin|>";
const K2_SECTION_CLOSE: &str = "<|tool_calls_section_end|>";
const K2_CALL_OPEN: &str = "<|tool_call_begin|>";
const K2_CALL_CLOSE: &str = "<|tool_call_end|>";
const K2_ARG_OPEN: &str = "<|tool_call_argument_begin|>";

/// Стандартный `tool_call_id`: `functions.name:0` или `name:0`. Голый счётчик
/// без имени (sglang угадывает по схеме) сюда не попадает — мы не угадываем.
static K2_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:functions\.)?(?P<name>[\w.\-]+):\d+$").expect("hardcoded regex is valid")
});

// --- Kimi K3 (XTML): <|open|>tools<|sep|>...<|open|>call tool="name" index="1"<|sep|>...<|close|>call<|sep|>...<|close|>tools<|sep|>
// Аргументы: <|open|>argument key="k" type="string"<|sep|>значение<|close|>argument<|sep|>

const K3_TOOLS_OPEN: &str = "<|open|>tools<|sep|>";
const K3_TOOLS_CLOSE: &str = "<|close|>tools<|sep|>";
const K3_CALL_OPEN: &str = "<|open|>call";
const K3_CALL_CLOSE: &str = "<|close|>call<|sep|>";
const K3_SEP: &str = "<|sep|>";
const K3_ARG_OPEN: &str = "<|open|>argument";
const K3_ARG_CLOSE: &str = "<|close|>argument<|sep|>";

/// Атрибут вида `key="value"` внутри `<|open|>call ...` / `<|open|>argument ...`.
static K3_ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?P<k>\w+)="(?P<v>[^"]*)""#).expect("hardcoded regex is valid"));

pub(super) fn families() -> Vec<Family> {
    vec![
        Family {
            opener: regex::escape(K2_SECTION_OPEN),
            parse: parse_k2,
        },
        Family {
            opener: regex::escape(K3_TOOLS_OPEN),
            parse: parse_k3,
        },
    ]
}

fn canonical(name: &str) -> String {
    format!(
        "Re-send the call as <tool_use><name>{name}</name><arguments>{{ valid JSON }}</arguments></tool_use>."
    )
}

// ---------- Kimi K2 ----------

fn parse_k2(hay: &str, at: usize) -> Option<Found> {
    let from = at + K2_SECTION_OPEN.len();
    // Процитированная обёртка без вызова следом — проза.
    if !hay[skip_ws(hay, from)..].starts_with(K2_CALL_OPEN) {
        return None;
    }
    let mut items = Vec::new();
    let mut pos = from;
    loop {
        let next_call = hay[pos..].find(K2_CALL_OPEN).map(|i| pos + i);
        let next_close = hay[pos..].find(K2_SECTION_CLOSE).map(|i| pos + i);
        let call_pos = match (next_call, next_close) {
            (Some(c), Some(s)) if c < s => c,
            (Some(c), None) => c,
            (_, Some(s)) => {
                return Some(Found {
                    span: at..s + K2_SECTION_CLOSE.len(),
                    items,
                });
            }
            (None, None) => {
                return Some(Found {
                    span: at..hay.len(),
                    items,
                });
            }
        };
        let (result, end) = k2_call(hay, call_pos);
        let broken = result.is_err();
        items.push(result);
        pos = end;
        if broken {
            return Some(Found {
                span: at..hay.len(),
                items,
            });
        }
    }
}

fn k2_call(hay: &str, pos: usize) -> (Result<RawCall, String>, usize) {
    let id_start = pos + K2_CALL_OPEN.len();
    let Some(arg_at) = hay[id_start..].find(K2_ARG_OPEN).map(|i| id_start + i) else {
        return (Err(k2_cut_off()), hay.len());
    };
    let id = hay[id_start..arg_at].trim();
    let Some(caps) = K2_ID_RE.captures(id) else {
        return (Err(k2_no_name(id)), hay.len());
    };
    let name = caps["name"].to_string();
    let args_start = skip_ws(hay, arg_at + K2_ARG_OPEN.len());
    match json_at(hay, args_start) {
        Ok((value, json_end)) if value.is_object() => {
            let after = skip_ws(hay, json_end);
            if !hay[after..].starts_with(K2_CALL_CLOSE) {
                return (Err(k2_trailing(&name)), hay.len());
            }
            let end = after + K2_CALL_CLOSE.len();
            (
                Ok(RawCall {
                    name,
                    args: RawArgs::Json(value),
                    format: "kimi_k2",
                }),
                end,
            )
        }
        Ok(_) => (
            Err(k2_bad_json(&name, "the arguments are not a JSON object")),
            hay.len(),
        ),
        Err(error) => (Err(k2_bad_json(&name, &error.to_string())), hay.len()),
    }
}

fn k2_cut_off() -> String {
    format!(
        "a Kimi K2 <|tool_call_begin|> block was cut off before its \
         <|tool_call_argument_begin|> marker. {}",
        canonical("TOOL")
    )
}

fn k2_no_name(id: &str) -> String {
    format!(
        "a Kimi K2 tool call id `{id}` has no function name (expected \
         `functions.<name>:<index>`). {}",
        canonical("TOOL")
    )
}

fn k2_trailing(name: &str) -> String {
    format!(
        "tool `{name}`: unexpected text after the Kimi K2 call arguments JSON. {}",
        canonical(name)
    )
}

fn k2_bad_json(name: &str, why: &str) -> String {
    format!("tool `{name}`: {why}. {}", canonical(name))
}

// ---------- Kimi K3 ----------

fn parse_k3(hay: &str, at: usize) -> Option<Found> {
    let from = at + K3_TOOLS_OPEN.len();
    if !hay[skip_ws(hay, from)..].starts_with(K3_CALL_OPEN) {
        return None;
    }
    let mut items = Vec::new();
    let mut pos = from;
    loop {
        let next_call = hay[pos..].find(K3_CALL_OPEN).map(|i| pos + i);
        let next_close = hay[pos..].find(K3_TOOLS_CLOSE).map(|i| pos + i);
        let call_pos = match (next_call, next_close) {
            (Some(c), Some(s)) if c < s => c,
            (Some(c), None) => c,
            (_, Some(s)) => {
                return Some(Found {
                    span: at..s + K3_TOOLS_CLOSE.len(),
                    items,
                });
            }
            (None, None) => {
                return Some(Found {
                    span: at..hay.len(),
                    items,
                });
            }
        };
        let (result, end) = k3_call(hay, call_pos);
        let broken = result.is_err();
        items.push(result);
        pos = end;
        if broken {
            return Some(Found {
                span: at..hay.len(),
                items,
            });
        }
    }
}

fn k3_call(hay: &str, pos: usize) -> (Result<RawCall, String>, usize) {
    let header_start = pos + K3_CALL_OPEN.len();
    let Some(sep_at) = hay[header_start..].find(K3_SEP).map(|i| header_start + i) else {
        return (Err(k3_cut_off("call")), hay.len());
    };
    let attrs = &hay[header_start..sep_at];
    let body_start = sep_at + K3_SEP.len();
    let Some(close_at) = hay[body_start..]
        .find(K3_CALL_CLOSE)
        .map(|i| body_start + i)
    else {
        return (Err(k3_cut_off("call")), hay.len());
    };
    let end = close_at + K3_CALL_CLOSE.len();
    let name = match k3_attr(attrs, "tool") {
        Some(name) if !name.is_empty() => name,
        _ => return (Err(k3_missing_attr("call", "tool")), hay.len()),
    };
    match k3_args(&hay[body_start..close_at]) {
        Ok(params) => (
            Ok(RawCall {
                name,
                args: RawArgs::Params(params),
                format: "kimi_k3",
            }),
            end,
        ),
        Err(error) => (Err(error), hay.len()),
    }
}

fn k3_args(body: &str) -> Result<Vec<Param>, String> {
    let mut params = Vec::new();
    let mut pos = 0;
    while let Some(open_rel) = body[pos..].find(K3_ARG_OPEN) {
        let header_start = pos + open_rel + K3_ARG_OPEN.len();
        let Some(sep_at) = body[header_start..].find(K3_SEP).map(|i| header_start + i) else {
            return Err(k3_cut_off("argument"));
        };
        let attrs = &body[header_start..sep_at];
        let value_start = sep_at + K3_SEP.len();
        let Some(close_at) = body[value_start..]
            .find(K3_ARG_CLOSE)
            .map(|i| value_start + i)
        else {
            return Err(k3_cut_off("argument"));
        };
        let key = match k3_attr(attrs, "key") {
            Some(key) if !key.is_empty() => key,
            _ => return Err(k3_missing_attr("argument", "key")),
        };
        let is_string = k3_attr(attrs, "type").is_none_or(|t| t == "string");
        params.push(Param {
            name: key,
            text: body[value_start..close_at].to_string(),
            declared: if is_string {
                Declared::Text
            } else {
                Declared::Json
            },
        });
        pos = close_at + K3_ARG_CLOSE.len();
    }
    Ok(params)
}

fn k3_attr(attrs: &str, key: &str) -> Option<String> {
    K3_ATTR_RE
        .captures_iter(attrs)
        .find(|c| &c["k"] == key)
        .map(|c| k3_unescape(&c["v"]))
}

/// Обратное экранирование атрибутов шаблона: `&quot;` раньше `&amp;`.
fn k3_unescape(value: &str) -> String {
    value.replace("&quot;", "\"").replace("&amp;", "&")
}

fn k3_cut_off(block: &str) -> String {
    format!(
        "a Kimi K3 <|open|>{block} ...<|sep|> block was cut off before its closing marker. {}",
        canonical("TOOL")
    )
}

fn k3_missing_attr(block: &str, attr: &str) -> String {
    format!(
        "a Kimi K3 <|open|>{block} ...<|sep|> block has no {attr}=\"...\" attribute. {}",
        canonical("TOOL")
    )
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::parse_with;
    use serde_json::json;

    fn tools() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            (
                "get_weather",
                json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            ),
            ("bash", json!({"type": "object"})),
        ]
    }

    // Kimi K2, форма из докстроки sglang kimik2_detector.py (стандартная),
    // с двумя вызовами в одной секции.
    #[test]
    fn k2_two_calls_in_one_section_parse() {
        let text = "<|tool_calls_section_begin|><|tool_call_begin|>functions.get_weather:0<|tool_call_argument_begin|>{\"city\": \"Tokyo\"}<|tool_call_end|>\n<|tool_call_begin|>functions.bash:1<|tool_call_argument_begin|>{}<|tool_call_end|>\n<|tool_calls_section_end|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].arguments, json!({"city": "Tokyo"}));
        assert_eq!(reply.calls[1].name, "bash");
        assert_eq!(reply.visible, "");
    }

    /// Без приставки `functions.` — тоже валидный `tool_call_id` (sglang
    /// `tool_call_id_regex`).
    #[test]
    fn k2_id_without_functions_prefix_parses() {
        let text = "<|tool_calls_section_begin|><|tool_call_begin|>bash:0<|tool_call_argument_begin|>{}<|tool_call_end|><|tool_calls_section_end|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "bash");
    }

    /// Голый счётчик без имени: sglang угадывает по схеме, мы — нет.
    #[test]
    fn k2_bare_counter_without_a_name_is_an_error_not_a_guess() {
        let text = "<|tool_calls_section_begin|><|tool_call_begin|>0<|tool_call_argument_begin|>{\"city\": \"Tokyo\"}<|tool_call_end|><|tool_calls_section_end|>";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
        assert!(reply.errors[0].contains('0'));
    }

    #[test]
    fn k2_marker_quoted_in_prose_is_not_a_call() {
        let text = "Kimi wraps calls in <|tool_calls_section_begin|> sections.";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    #[test]
    fn k2_cut_off_call_is_reported_not_dropped() {
        let text = "<|tool_calls_section_begin|><|tool_call_begin|>functions.bash:0<|tool_call_argument_begin|>{\"a";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    // Kimi K3 (XTML), форма из докстроки vLLM kimi_k3_tool_parser.py / sglang
    // kimik3_detector.py: канал tools с одним call и двумя argument.
    #[test]
    fn k3_call_with_string_and_object_arguments_parses() {
        let text = "<|open|>tools<|sep|>\n<|open|>call tool=\"bash\" index=\"1\"<|sep|>\n<|open|>argument key=\"code\" type=\"string\"<|sep|>print(1)<|close|>argument<|sep|>\n<|open|>argument key=\"opts\" type=\"object\"<|sep|>{\"a\":1}<|close|>argument<|sep|>\n<|close|>call<|sep|>\n<|close|>tools<|sep|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "bash");
        assert_eq!(
            reply.calls[0].arguments,
            json!({"code": "print(1)", "opts": {"a": 1}})
        );
        assert_eq!(reply.visible, "");
    }

    /// `type` отсутствует — по умолчанию строка (sglang `arg_attrs.get("type", "string")`).
    #[test]
    fn k3_argument_without_a_type_attribute_defaults_to_string() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"get_weather\"<|sep|><|open|>argument key=\"city\"<|sep|>Paris<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
    }

    /// Экранирование в атрибутах: `&quot;`/`&amp;`, не в значении аргумента.
    #[test]
    fn k3_attribute_escaping_is_reversed_but_the_value_stays_raw() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"bash\"<|sep|><|open|>argument key=\"a&amp;b\" type=\"string\"<|sep|>x &amp; y<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls[0].arguments, json!({"a&b": "x &amp; y"}));
    }

    /// Несколько вызовов в одном канале `tools`.
    #[test]
    fn k3_two_calls_in_one_tools_channel_parse() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"bash\"<|sep|><|close|>call<|sep|><|open|>call tool=\"get_weather\"<|sep|><|open|>argument key=\"city\" type=\"string\"<|sep|>Rome<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>";
        let reply = parse_with(text, &tools());
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "bash");
        assert_eq!(reply.calls[1].arguments, json!({"city": "Rome"}));
    }

    /// Канал рассуждений `<|open|>think<|sep|>` — не вызов, не наш маркер.
    #[test]
    fn k3_think_channel_alone_is_not_a_call() {
        let text = "<|open|>think<|sep|>maybe bash<|close|>think<|sep|>Done.";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    /// Вызов без `tool=` — форма явно наша (`<|open|>call ...<|sep|>`), но
    /// сломана: диагностика, а не молчаливый пропуск (в отличие от sglang/vLLM).
    #[test]
    fn k3_call_without_a_tool_attribute_is_reported() {
        let text = "<|open|>tools<|sep|><|open|>call index=\"1\"<|sep|><|close|>call<|sep|><|close|>tools<|sep|>";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    #[test]
    fn k3_marker_quoted_in_prose_is_not_a_call() {
        let text = "K3 opens a <|open|>tools<|sep|> channel for calls.";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    #[test]
    fn k3_cut_off_call_is_reported_not_dropped() {
        let text = "<|open|>tools<|sep|><|open|>call tool=\"bash\"<|sep|><|open|>argument key=\"a\" type=\"string\"<|sep|>x";
        let reply = parse_with(text, &tools());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }
}
