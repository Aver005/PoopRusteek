//! Семейство «маркер + JSON»: тело — значение JSON целиком, конец ищет разбор
//! (`json_at`); закрывающий маркер необязателен — только прячется из ответа.

use super::{Family, Found, RawArgs, RawCall, json_at, skip_ws};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";
/// Granite 3.0: спецтокен с чертами; тело — массив, закрытия нет вовсе.
const GRANITE3_OPEN: &str = "<|tool_call|>";
const LONGCAT_OPEN: &str = "<longcat_tool_call>";
const LONGCAT_CLOSE: &str = "</longcat_tool_call>";
/// Jamba и Hunyuan-A13B: одна и та же обёртка, массив вызовов.
const TOOL_CALLS_OPEN: &str = "<tool_calls>";
const TOOL_CALLS_CLOSE: &str = "</tool_calls>";
const FUNCTION_CALL_OPEN: &str = "<function_call>";
const COHERE_OPEN: &str = "<|START_ACTION|>";
const COHERE_CLOSE: &str = "<|END_ACTION|>";
const INKLING_OPEN: &str = "<|content_invoke_tool_json|>";
const INKLING_CLOSE: &str = "<|end_message|>";
const INTERNLM_OPEN_PATTERN: &str = r"<\|action_start\|>\s*<\|plugin\|>";
const INTERNLM_CLOSE: &str = "<|action_end|>";
const APERTUS_OPEN: &str = "<|tools_prefix|>";
const APERTUS_CLOSE: &str = "<|tools_suffix|>";
const MISTRAL_OPEN: &str = "[TOOL_CALLS]";
const MISTRAL_ARGS: &str = "[ARGS]";
const FUNCTOOLS_OPEN: &str = "functools";
const GIGACHAT_A_OPEN: &str = "<|function_call|>";
const GIGACHAT_B_OPEN: &str = "function call<|role_sep|>\n";
const DOTS_OPEN: &str = "<dots_function_call>";
const DOTS_CLOSE: &str = "</dots_function_call>";
const PYTHON_TAG_OPEN: &str = "<|python_tag|>";
/// Ответ OpenAI, процитированный как текст: маркер — сама открывающая `{`.
const OPENAI_RESPONSE_PATTERN: &str = r#"\{\s*"tool_calls"\s*:\s*\["#;

static INTERNLM_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(INTERNLM_OPEN_PATTERN).expect("hardcoded regex is valid"));

pub(super) fn families() -> Vec<Family> {
    vec![
        Family {
            opener: regex::escape(TOOL_CALL_OPEN),
            parse: parse_hermes,
        },
        Family {
            opener: regex::escape(GRANITE3_OPEN),
            parse: parse_granite3,
        },
        Family {
            opener: regex::escape(LONGCAT_OPEN),
            parse: parse_longcat,
        },
        Family {
            opener: regex::escape(TOOL_CALLS_OPEN),
            parse: parse_jamba,
        },
        Family {
            opener: regex::escape(FUNCTION_CALL_OPEN),
            parse: parse_granite_20b_fc,
        },
        Family {
            opener: regex::escape(COHERE_OPEN),
            parse: parse_cohere,
        },
        Family {
            opener: regex::escape(INKLING_OPEN),
            parse: parse_inkling,
        },
        Family {
            opener: INTERNLM_OPEN_PATTERN.to_string(),
            parse: parse_internlm,
        },
        Family {
            opener: regex::escape(APERTUS_OPEN),
            parse: parse_apertus,
        },
        Family {
            opener: regex::escape(MISTRAL_OPEN),
            parse: parse_mistral,
        },
        Family {
            opener: regex::escape(FUNCTOOLS_OPEN),
            parse: parse_phi4mini,
        },
        Family {
            opener: regex::escape(GIGACHAT_A_OPEN),
            parse: parse_gigachat3_a,
        },
        Family {
            opener: regex::escape(GIGACHAT_B_OPEN),
            parse: parse_gigachat3_b,
        },
        Family {
            opener: regex::escape(DOTS_OPEN),
            parse: parse_dots,
        },
        Family {
            opener: regex::escape(PYTHON_TAG_OPEN),
            parse: parse_llama,
        },
        Family {
            opener: OPENAI_RESPONSE_PATTERN.to_string(),
            parse: parse_openai_response,
        },
    ]
}

/// Имя вызова из общих ключей тела: `name`, `tool`, `function` (строкой или
/// вложенным `function.name`), `tool_name`.
fn call_name(value: &Value) -> Option<String> {
    for key in ["name", "tool"] {
        if let Some(name) = value.get(key).and_then(Value::as_str) {
            return Some(name.to_string());
        }
    }
    match value.get("function") {
        Some(Value::String(name)) => return Some(name.clone()),
        Some(Value::Object(func)) => {
            if let Some(name) = func.get("name").and_then(Value::as_str) {
                return Some(name.to_string());
            }
        }
        _ => {}
    }
    value
        .get("tool_name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

const ARG_KEYS: [&str; 5] = ["arguments", "parameters", "args", "input", "params"];

/// Аргументы из общих ключей тела, на верхнем уровне или внутри `function`
/// (ответ OpenAI кладёт их именно туда, строкой).
fn call_args(value: &Value) -> Value {
    for key in ARG_KEYS {
        if let Some(v) = value.get(key) {
            return v.clone();
        }
    }
    if let Some(Value::Object(func)) = value.get("function") {
        for key in ARG_KEYS {
            if let Some(v) = func.get(key) {
                return v.clone();
            }
        }
    }
    Value::Object(Default::default())
}

fn call_from_object(value: Value, format: &'static str) -> Result<RawCall, String> {
    let Some(name) = call_name(&value) else {
        return Err(format!(
            "a `{format}` tool call had no \"name\"/\"tool\"/\"function\" field. Re-send it as \
             <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
        ));
    };
    Ok(RawCall {
        name,
        args: RawArgs::Json(call_args(&value)),
        format,
    })
}

/// Элемент Apertus: единственный ключ объекта — имя инструмента, значение —
/// аргументы целиком (не под `arguments`).
fn apertus_item(value: Value, format: &'static str) -> Result<RawCall, String> {
    match value {
        Value::Object(map) if map.len() == 1 => {
            let (name, args) = map.into_iter().next().expect("checked len == 1");
            Ok(RawCall {
                name,
                args: RawArgs::Json(args),
                format,
            })
        }
        _ => Err(format!(
            "an `{format}` tool call element must be a single-key object \
             ({{\"tool_name\": {{ arguments }}}})."
        )),
    }
}

/// Тело marker+JSON: одно значение (объект — вызов, массив — вызов на
/// элемент); закрывающий маркер, если есть, только прячется из ответа.
fn tag_json(
    hay: &str,
    at: usize,
    body_start: usize,
    close: &str,
    format: &'static str,
    extract: fn(Value, &'static str) -> Result<RawCall, String>,
) -> Option<Found> {
    let body = skip_ws(hay, body_start);
    if !hay[body..].starts_with(['{', '[']) {
        return None;
    }
    let (value, mut end) = match json_at(hay, body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(Found::broken(
                at..hay.len(),
                format!(
                    "a `{format}` tool call body is not valid JSON ({error}). Re-send the call \
                     as <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
                ),
            ));
        }
    };
    let after = skip_ws(hay, end);
    if !close.is_empty() && hay[after..].starts_with(close) {
        end = after + close.len();
    }
    let items = match value {
        Value::Array(items) => items.into_iter().map(|v| extract(v, format)).collect(),
        object => vec![extract(object, format)],
    };
    Some(Found {
        span: at..end,
        items,
    })
}

/// Hermes, Qwen2.5, Granite 3.1, Ernie 4.5, Granite 4 — один маркер, одно
/// тело (объект или массив), закрытие необязательно.
fn parse_hermes(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + TOOL_CALL_OPEN.len(),
        TOOL_CALL_CLOSE,
        "hermes",
        call_from_object,
    )
}

fn parse_granite3(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + GRANITE3_OPEN.len(),
        "",
        "granite3",
        call_from_object,
    )
}

fn parse_longcat(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + LONGCAT_OPEN.len(),
        LONGCAT_CLOSE,
        "longcat",
        call_from_object,
    )
}

fn parse_jamba(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + TOOL_CALLS_OPEN.len(),
        TOOL_CALLS_CLOSE,
        "jamba",
        call_from_object,
    )
}

fn parse_granite_20b_fc(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + FUNCTION_CALL_OPEN.len(),
        "",
        "granite_20b_fc",
        call_from_object,
    )
}

fn parse_cohere(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + COHERE_OPEN.len(),
        COHERE_CLOSE,
        "cohere",
        call_from_object,
    )
}

fn parse_inkling(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + INKLING_OPEN.len(),
        INKLING_CLOSE,
        "inkling",
        call_from_object,
    )
}

fn parse_internlm(hay: &str, at: usize) -> Option<Found> {
    let m = INTERNLM_OPEN_RE
        .find_at(hay, at)
        .filter(|m| m.start() == at)?;
    tag_json(
        hay,
        at,
        m.end(),
        INTERNLM_CLOSE,
        "internlm",
        call_from_object,
    )
}

fn parse_apertus(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + APERTUS_OPEN.len(),
        APERTUS_CLOSE,
        "apertus",
        apertus_item,
    )
}

fn parse_phi4mini(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + FUNCTOOLS_OPEN.len(),
        "",
        "phi4mini",
        call_from_object,
    )
}

fn parse_gigachat3_a(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + GIGACHAT_A_OPEN.len(),
        "",
        "gigachat3",
        call_from_object,
    )
}

fn parse_gigachat3_b(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + GIGACHAT_B_OPEN.len(),
        "",
        "gigachat3",
        call_from_object,
    )
}

fn parse_dots(hay: &str, at: usize) -> Option<Found> {
    tag_json(
        hay,
        at,
        at + DOTS_OPEN.len(),
        DOTS_CLOSE,
        "dots",
        call_from_object,
    )
}

/// Llama 3.x: объекты подряд через `;`, аргументы — `arguments` или
/// `parameters` (sglang `llama32_detector.py`).
fn parse_llama(hay: &str, at: usize) -> Option<Found> {
    let mut pos = skip_ws(hay, at + PYTHON_TAG_OPEN.len());
    if !hay[pos..].starts_with('{') {
        return None;
    }
    let mut items = Vec::new();
    loop {
        match json_at(hay, pos) {
            Ok((value, end)) => {
                items.push(call_from_object(value, "llama"));
                pos = end;
            }
            Err(error) => {
                items.push(Err(format!(
                    "a `llama` tool call body is not valid JSON ({error}). Re-send the call as \
                     <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
                )));
                return Some(Found {
                    span: at..hay.len(),
                    items,
                });
            }
        }
        let sep = skip_ws(hay, pos);
        let Some(after_sep) = hay[sep..].strip_prefix(';').map(|_| skip_ws(hay, sep + 1)) else {
            break;
        };
        if !hay[after_sep..].starts_with('{') {
            break;
        }
        pos = after_sep;
    }
    Some(Found {
        span: at..pos,
        items,
    })
}

/// Mistral: массив `[TOOL_CALLS] […]` или компакт `[TOOL_CALLS]имя[ARGS]{…}`
/// / `[TOOL_CALLS]имя{…}` — `{` тоже разделяет имя и аргументы (vLLM `parser/mistral.py`).
fn parse_mistral(hay: &str, at: usize) -> Option<Found> {
    let probe = skip_ws(hay, at + MISTRAL_OPEN.len());
    if hay[probe..].starts_with('[') {
        return mistral_array(hay, at, probe);
    }
    mistral_compact(hay, at, probe)
}

fn mistral_array(hay: &str, at: usize, body: usize) -> Option<Found> {
    let (value, end) = match json_at(hay, body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(Found::broken(
                at..hay.len(),
                format!(
                    "a `mistral` [TOOL_CALLS] array is not valid JSON ({error}). Re-send the \
                     call as <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
                ),
            ));
        }
    };
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    Some(Found {
        span: at..end,
        items: items
            .into_iter()
            .map(|v| call_from_object(v, "mistral"))
            .collect(),
    })
}

fn mistral_compact(hay: &str, at: usize, name_start: usize) -> Option<Found> {
    let rest = &hay[name_start..];
    let name_end = match (rest.find(MISTRAL_ARGS), rest.find('{')) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    if name_end == 0 {
        return None;
    }
    let name = rest[..name_end].trim().to_string();
    let mut body = name_start + name_end;
    if hay[body..].starts_with(MISTRAL_ARGS) {
        body = skip_ws(hay, body + MISTRAL_ARGS.len());
    }
    if !hay[body..].starts_with('{') {
        return Some(Found::broken(
            at..hay.len(),
            format!(
                "tool `{name}`: a Mistral [TOOL_CALLS] call has no JSON arguments. Re-send it as \
                 <tool_use><name>{name}</name><arguments>{{ valid JSON }}</arguments></tool_use>."
            ),
        ));
    }
    match json_at(hay, body) {
        Ok((value, end)) => Some(Found::call(
            at..end,
            RawCall {
                name,
                args: RawArgs::Json(value),
                format: "mistral",
            },
        )),
        Err(error) => Some(Found::broken(
            at..hay.len(),
            format!(
                "tool `{name}`: Mistral [TOOL_CALLS] arguments are not valid JSON ({error}). \
                 Re-send the call as <tool_use><name>{name}</name><arguments>{{ valid JSON }}</arguments></tool_use>."
            ),
        )),
    }
}

/// Ответ OpenAI, процитированный как текст: `{"tool_calls":[{"function":
/// {"name":…, "arguments": "<строка JSON>"}}]}`. `at` — сама открывающая `{`.
fn parse_openai_response(hay: &str, at: usize) -> Option<Found> {
    let (value, end) = match json_at(hay, at) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(Found::broken(
                at..hay.len(),
                format!(
                    "an OpenAI-shaped \"tool_calls\" object is not valid JSON ({error}). \
                     Re-send the call as <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
                ),
            ));
        }
    };
    let calls = value
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty());
    let Some(calls) = calls else {
        return Some(Found::broken(
            at..end,
            "an OpenAI-shaped object had no non-empty \"tool_calls\" array.".into(),
        ));
    };
    let items = calls
        .iter()
        .cloned()
        .map(|call| call_from_object(call, "openai_response"))
        .collect();
    Some(Found {
        span: at..end,
        items,
    })
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::{parse_reply, parse_with};
    use serde_json::json;

    /// По форме sglang `qwen25_detector.py` (bot/eot с переводом строки) и
    /// vLLM `hermes_tool_parser.py`: несколько вызовов подряд.
    #[test]
    fn a_hermes_object_call_parses_and_several_run_in_sequence() {
        let text = "<tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Tokyo\"}}\n</tool_call>\n<tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Paris\"}}\n</tool_call>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "Tokyo"}));
        assert_eq!(reply.calls[1].arguments, json!({"city": "Paris"}));
        assert_eq!(reply.visible, "");
    }

    /// vLLM `hermes_tool_parser.py`: `tool_call_regex` матчит и без закрытия.
    #[test]
    fn a_hermes_call_without_a_closing_tag_still_parses() {
        let text = "<tool_call>{\"name\": \"ls\", \"arguments\": {}}";
        let reply = parse_with(text, &[("ls", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "ls");
    }

    /// Дословно из docstring vLLM `parser/granite.py`: Granite 3.1, массив,
    /// закрытия нет.
    #[test]
    fn a_granite_3_1_array_parses_without_closing() {
        let text = "<tool_call> [{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}]";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "SF"}));
    }

    /// Тот же docstring: Granite 3.0, спецтокен с чертами, два вызова.
    #[test]
    fn a_granite_3_0_array_with_two_calls_parses() {
        let text = "<|tool_call|> [{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}, {\"name\": \"get_time\", \"arguments\": {}}]";
        let reply = parse_with(text, &[("get_weather", json!({})), ("get_time", json!({}))]);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[1].name, "get_time");
    }

    /// Ernie 4.5 (`ernie45_tool_parser.py`) и Granite 4 (`granite4_tool_parser.py`)
    /// используют тот же маркер и тело, что Hermes — своей грамматики не нужно.
    #[test]
    fn ernie45_and_granite4_share_the_hermes_grammar() {
        let text =
            "<tool_call>{\"name\": \"search\", \"arguments\": {\"q\": \"rust\"}}</tool_call>";
        let reply = parse_with(text, &[("search", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
    }

    /// По устройству vLLM `longcat_tool_parser.py` (подкласс Hermes2Pro со
    /// своими токенами).
    #[test]
    fn a_longcat_call_parses() {
        let text = "<longcat_tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}</longcat_tool_call>";
        let reply = parse_with(text, &[("bash", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["command"], "ls");
    }

    /// По форме vLLM `jamba_tool_parser.py` / `hunyuan_a13b_tool_parser.py`:
    /// массив вызовов в одной обёртке `<tool_calls>`.
    #[test]
    fn a_tool_calls_array_parses_jamba_and_hunyuan_a13b() {
        let text = "<tool_calls>[{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}, {\"name\": \"get_time\", \"arguments\": {}}]</tool_calls>";
        let reply = parse_with(text, &[("get_weather", json!({})), ("get_time", json!({}))]);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
    }

    /// По форме vLLM `granite_20b_fc_tool_parser.py`: без закрытия, несколько
    /// подряд — `dec.raw_decode` находит конец каждого объекта.
    #[test]
    fn granite_20b_fc_calls_run_back_to_back_without_closing() {
        let text = "<function_call>{\"name\": \"a\", \"arguments\": {}}<function_call>{\"name\": \"b\", \"arguments\": {}}";
        let reply = parse_with(text, &[("a", json!({})), ("b", json!({}))]);
        let names: Vec<_> = reply.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b"], "{:?}", reply.errors);
    }

    /// Дословно по шаблону `structure_info` в sglang `cohere_command4_detector.py`.
    #[test]
    fn a_cohere_action_array_parses() {
        let text = "<|START_ACTION|>[{\"tool_call_id\": \"0\", \"tool_name\": \"get_weather\", \"parameters\": {\"city\": \"SF\"}}]<|END_ACTION|>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "SF"}));
    }

    /// По шаблону `structure_info` в sglang `inkling_detector.py`.
    #[test]
    fn an_inkling_call_parses() {
        let text = "<|content_invoke_tool_json|>{\"name\":\"get_weather\",\"args\":{\"city\":\"SF\"}}<|end_message|>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "SF"}));
    }

    /// Дословно по примеру из docstring sglang `internlm_detector.py`.
    #[test]
    fn an_internlm_call_parses() {
        let text = "What's the weather like?<|action_start|> <|plugin|>\n{\"name\": \"get_weather\", \"parameters\": {\"location\": \"Tokyo\"}}<|action_end|>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"location": "Tokyo"}));
        assert_eq!(reply.visible, "What's the weather like?");
    }

    /// vLLM `internlm2_tool_parser.py`: без пробела между `<|action_start|>` и `<|plugin|>`.
    #[test]
    fn an_internlm_call_without_a_space_before_plugin_parses() {
        let text = "<|action_start|><|plugin|>\n{\"name\": \"get_weather\", \"parameters\": {\"location\": \"Tokyo\"}}<|action_end|>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"location": "Tokyo"}));
    }

    /// Дословно из docstring sglang `apertus2509_detector.py`.
    #[test]
    fn an_apertus_call_parses_the_single_key_shape() {
        let text = "<|tools_prefix|>[{\"get_weather\": {\"city\": \"Paris\"}}]<|tools_suffix|>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
    }

    /// Дословно: `bot_token` sglang `mistral_detector.py`.
    #[test]
    fn a_mistral_array_call_parses() {
        let text = "[TOOL_CALLS] [{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}]";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
    }

    /// Компакт с явным `[ARGS]` (sglang `mistral_detector.py`, v11+).
    #[test]
    fn a_mistral_compact_args_call_parses() {
        let text = "[TOOL_CALLS]get_weather[ARGS]{\"city\": \"SF\"}";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "SF"}));
    }

    /// Компакт без `[ARGS]`: `{` сама разделяет имя и аргументы (vLLM
    /// `parser/mistral.py`).
    #[test]
    fn a_mistral_compact_brace_call_parses() {
        let text = "[TOOL_CALLS]get_weather{\"city\": \"SF\"}";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
    }

    /// Дословно: `pattern` в vLLM `phi4mini_tool_parser.py`.
    #[test]
    fn a_phi4mini_functools_call_parses() {
        let text = "functools[{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}]";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
    }

    /// Дословно: оба маркера из `has_tool_call` в sglang `gigachat3_detector.py`.
    #[test]
    fn a_gigachat3_call_parses_with_either_marker() {
        for text in [
            "<|function_call|>{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}",
            "function call<|role_sep|>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}}",
        ] {
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert_eq!(reply.calls.len(), 1, "{text}: {:?}", reply.errors);
            assert_eq!(reply.calls[0].arguments, json!({"city": "SF"}), "{text}");
        }
    }

    /// Ответ OpenAI, процитированный как текст: аргументы строкой.
    #[test]
    fn an_openai_response_shaped_object_parses_string_arguments() {
        let text = "{\"tool_calls\":[{\"id\":\"1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]}";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"city": "SF"}));
    }

    /// dots: запасной JSON-вариант тела (XML `<invoke>` уже разобран в
    /// invoke.rs — тот же маркер, выбор по первому символу тела).
    #[test]
    fn a_dots_json_fallback_call_parses() {
        let text = "<dots_function_call>{\"name\": \"search\", \"arguments\": {\"query\": \"weather\"}}</dots_function_call>";
        let reply = parse_with(text, &[("search", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["query"], "weather");
    }

    /// По форме sglang `llama32_detector.py`: объекты через `;`, ключ
    /// `parameters` вместо `arguments`.
    #[test]
    fn llama_python_tag_calls_separated_by_semicolons_parse() {
        let text = "<|python_tag|>{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}};{\"name\": \"get_time\", \"parameters\": {}}";
        let reply = parse_with(text, &[("get_weather", json!({})), ("get_time", json!({}))]);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[1].arguments, json!({}));
    }

    /// vLLM `llama3_json_tool_parser.py`: пробелы вокруг `;` тоже разделяют.
    #[test]
    fn llama_semicolons_with_spaces_around_them_parse() {
        let text = "<|python_tag|>{\"name\": \"get_weather\", \"arguments\": {\"city\": \"SF\"}} ; {\"name\": \"get_time\", \"parameters\": {}}";
        let reply = parse_with(text, &[("get_weather", json!({})), ("get_time", json!({}))]);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[1].arguments, json!({}));
    }

    /// Маркер, процитированный в прозе без тела вызова, — не вызов.
    #[test]
    fn markers_quoted_in_prose_are_not_calls() {
        for text in [
            "Wrap the call in <tool_call> tags.",
            "Mistral prints [TOOL_CALLS] before the array.",
            "The model may emit functools as a Python import.",
            "Cohere uses <|START_ACTION|> to start an action.",
        ] {
            let reply = parse_reply(text);
            assert!(reply.calls.is_empty() && reply.errors.is_empty(), "{text}");
            assert_eq!(reply.visible, text);
        }
    }

    #[test]
    fn a_broken_hermes_json_body_is_reported() {
        let reply = parse_reply("<tool_call>{\"name\": \"x\", \"arguments\": }</tool_call>");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn a_hermes_call_missing_a_name_is_reported() {
        let reply = parse_reply("<tool_call>{\"arguments\": {}}</tool_call>");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn a_broken_mistral_compact_body_is_reported() {
        let reply = parse_reply("[TOOL_CALLS]get_weather[ARGS]{\"city\": }");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }

    #[test]
    fn an_openai_response_without_a_tool_calls_array_is_reported() {
        let reply = parse_reply("{\"tool_calls\": []}");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }
}
