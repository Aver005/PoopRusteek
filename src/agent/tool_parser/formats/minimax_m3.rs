//! MiniMax M3: маркер `]<]minimax[>[` стоит перед каждым тегом; вложенность и
//! массивы через `<item>` разрешены. sglang `minimax_m3.py`; vLLM `minimax_m3_tool_parser.py` — обёртка над тем же разбором в Rust-крейте.

use super::{Family, Found, RawArgs, RawCall};
use serde_json::{Map, Value};

const MARKER: &str = "]<]minimax[>[";
const TOOL_CALL_OPEN: &str = "]<]minimax[>[<tool_call>";
const TOOL_CALL_CLOSE: &str = "]<]minimax[>[</tool_call>";
const INVOKE_PREFIX: &str = "]<]minimax[>[<invoke name=\"";
const INVOKE_CLOSE: &str = "]<]minimax[>[</invoke>";
const CANONICAL: &str =
    "<tool_use><name>TOOL</name><arguments>{ valid JSON }</arguments></tool_use>";

pub(super) fn families() -> Vec<Family> {
    vec![Family {
        opener: regex::escape(TOOL_CALL_OPEN),
        parse,
    }]
}

/// Обёртка `<tool_call>` с одним или несколькими `<invoke>`. Без закрытия
/// обёртки берём целые `invoke` до конца текста, как в DSML.
fn parse(hay: &str, at: usize) -> Option<Found> {
    let body_from = at + TOOL_CALL_OPEN.len();
    let close = hay[body_from..]
        .find(TOOL_CALL_CLOSE)
        .map(|i| body_from + i);
    let scan_end = close.unwrap_or(hay.len());
    // Обёртка процитирована без единого <invoke> внутри — проза, не вызов.
    let first = hay[body_from..scan_end].find(INVOKE_PREFIX)?;
    let mut items = Vec::new();
    let mut pos = body_from + first;
    loop {
        match parse_invoke(hay, pos, scan_end) {
            Ok((call, end)) => {
                items.push(Ok(call));
                pos = end;
                match hay[pos..].find(INVOKE_PREFIX) {
                    Some(offset) if close.is_some_and(|c| pos + offset >= c) => break,
                    Some(offset) => pos += offset,
                    None => break,
                }
            }
            Err(error) => {
                items.push(Err(error));
                return Some(Found {
                    span: at..hay.len(),
                    items,
                });
            }
        }
    }
    Some(match close {
        Some(close_start) => Found {
            span: at..close_start + TOOL_CALL_CLOSE.len(),
            items,
        },
        None => Found {
            span: at..pos,
            items,
        },
    })
}

/// Один `<invoke>`: имя в кавычках, потом дерево параметров до `</invoke>`.
/// `bound` — граница обёртки, чужой `</invoke>` за ней не считается.
fn parse_invoke(hay: &str, at: usize, bound: usize) -> Result<(RawCall, usize), String> {
    let name_from = at + INVOKE_PREFIX.len();
    let Some(name_len) = hay[name_from..bound.max(name_from)].find("\">") else {
        return Err(format!(
            "an <invoke> tag was cut off before its name attribute closed. Re-send \
             the call as {CANONICAL}."
        ));
    };
    let name = hay[name_from..name_from + name_len].to_string();
    let body_from = name_from + name_len + 2;
    let Some(body_len) = hay[body_from..bound.max(body_from)].find(INVOKE_CLOSE) else {
        return Err(format!(
            "tool `{name}`: the call was cut off before its closing tag. Re-send \
             the call as {CANONICAL}."
        ));
    };
    let body = &hay[body_from..body_from + body_len];
    let end = body_from + body_len + INVOKE_CLOSE.len();
    let args = parse_body(body)
        .map_err(|why| format!("tool `{name}`: {why}. Re-send the call as {CANONICAL}."))?;
    Ok((
        RawCall {
            name,
            args: RawArgs::Json(Value::Object(args)),
            format: "minimax_m3",
        },
        end,
    ))
}

struct Frame {
    tag: String,
    text: String,
    children: Vec<(String, Value)>,
}

/// Тело `<invoke>` по тегам-маркерам. Лист без детей — строка; подряд идущие
/// `<item>` — массив; иначе объект (повтор имени на уровне — тоже массив).
fn parse_body(body: &str) -> Result<Map<String, Value>, String> {
    let mut stack = vec![Frame {
        tag: String::new(),
        text: String::new(),
        children: Vec::new(),
    }];
    for raw in body.split(MARKER) {
        let chunk = raw.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some(rest) = chunk.strip_prefix("</") {
            let gt = rest.find('>').unwrap_or(rest.len());
            let tag = rest[..gt].trim();
            if stack.len() == 1 {
                return Err(format!("an unexpected closing tag `{tag}`"));
            }
            let frame = stack.pop().expect("checked len > 1 above");
            if frame.tag != tag {
                return Err(format!(
                    "a mismatched closing tag: expected `{}`, found `{tag}`",
                    frame.tag
                ));
            }
            let value = finish(frame.text, frame.children);
            stack
                .last_mut()
                .expect("root frame stays on the stack")
                .children
                .push((frame.tag, value));
        } else if let Some(rest) = chunk.strip_prefix('<') {
            let Some(gt) = rest.find('>') else {
                return Err("a malformed parameter tag".to_string());
            };
            stack.push(Frame {
                tag: rest[..gt].trim().to_string(),
                text: rest[gt + 1..].to_string(),
                children: Vec::new(),
            });
        } else {
            return Err(format!("stray text `{chunk}` outside of a parameter tag"));
        }
    }
    if stack.len() != 1 {
        return Err("a parameter tag was cut off before its closing tag".to_string());
    }
    Ok(merge(stack.pop().expect("checked len == 1 above").children))
}

fn finish(text: String, children: Vec<(String, Value)>) -> Value {
    if children.is_empty() {
        return Value::String(text.trim().to_string());
    }
    if children.iter().all(|(tag, _)| tag == "item") {
        Value::Array(children.into_iter().map(|(_, v)| v).collect())
    } else {
        Value::Object(merge(children))
    }
}

fn merge(children: Vec<(String, Value)>) -> Map<String, Value> {
    let mut object = Map::new();
    for (tag, value) in children {
        match object.get_mut(&tag) {
            Some(Value::Array(items)) => items.push(value),
            Some(existing) => {
                let previous = existing.take();
                *existing = Value::Array(vec![previous, value]);
            }
            None => {
                object.insert(tag, value);
            }
        }
    }
    object
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::parse_with;
    use serde_json::json;

    /// Собран по константам sglang `minimax_m3.py` (`MINIMAX_NS_TOKEN`,
    /// `TOOL_CALL_START/END`, `INVOKE_PREFIX/SUFFIX`) — там нет своего примера.
    #[test]
    fn a_minimax_m3_call_parses() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"get_weather\">]<]minimax[>[<city>Paris]<]minimax[>[</city>]<]minimax[>[</invoke>]<]minimax[>[</tool_call>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
        assert_eq!(reply.visible, "");
    }

    /// Несколько `<invoke>` в одной обёртке.
    #[test]
    fn several_invokes_in_one_wrapper_parse() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"a\">]<]minimax[>[<x>1]<]minimax[>[</x>]<]minimax[>[</invoke>]<]minimax[>[<invoke name=\"b\">]<]minimax[>[<y>2]<]minimax[>[</y>]<]minimax[>[</invoke>]<]minimax[>[</tool_call>";
        let reply = parse_with(text, &[("a", json!({})), ("b", json!({}))]);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"x": "1"}));
        assert_eq!(reply.calls[1].arguments, json!({"y": "2"}));
    }

    /// Вложенный объект и массив через повтор `<item>`.
    #[test]
    fn nested_object_and_item_array_parse() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"a\">\
]<]minimax[>[<opts>]<]minimax[>[<dry>true]<]minimax[>[</dry>]<]minimax[>[</opts>\
]<]minimax[>[<tags>]<]minimax[>[<item>x]<]minimax[>[</item>]<]minimax[>[<item>y]<]minimax[>[</item>]<]minimax[>[</tags>\
]<]minimax[>[</invoke>]<]minimax[>[</tool_call>";
        let reply = parse_with(text, &[("a", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"opts": {"dry": "true"}, "tags": ["x", "y"]})
        );
    }

    /// Без обёртки, без закрытия — обычная проза про формат.
    #[test]
    fn a_bare_marker_mention_without_a_call_is_prose() {
        let text = "MiniMax M3 wraps calls in ]<]minimax[>[<tool_call> tags.";
        let reply = parse_with(text, &[("a", json!({}))]);
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }

    /// Оборванный вызов без закрывающего `</invoke>` — ошибка, не догадка.
    #[test]
    fn a_cut_off_invoke_is_reported() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"a\">]<]minimax[>[<x>1";
        let reply = parse_with(text, &[("a", json!({}))]);
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Обёртка не закрыта, но сам `<invoke>` целый — принимается, как DSML.
    #[test]
    fn an_unclosed_wrapper_with_a_whole_invoke_still_runs() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"a\">]<]minimax[>[<x>1]<]minimax[>[</x>]<]minimax[>[</invoke>";
        let reply = parse_with(text, &[("a", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({"x": "1"}));
    }

    /// Несовпадающий закрывающий тег внутри тела — диагностика.
    #[test]
    fn a_mismatched_closing_tag_is_reported() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"a\">]<]minimax[>[<x>1]<]minimax[>[</y>]<]minimax[>[</invoke>]<]minimax[>[</tool_call>";
        let reply = parse_with(text, &[("a", json!({}))]);
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Незнакомое имя в этом формате идёт через человека (недоверенный формат).
    #[test]
    fn the_call_needs_a_person_and_an_unknown_name_stays_prose() {
        let text = "]<]minimax[>[<tool_call>]<]minimax[>[<invoke name=\"get_weather\">]<]minimax[>[<city>Paris]<]minimax[>[</city>]<]minimax[>[</invoke>]<]minimax[>[</tool_call>";
        let reply = parse_with(text, &[("get_weather", json!({}))]);
        assert!(reply.calls[0].origin.needs_person());

        let unknown = parse_with(text, &[("other_tool", json!({}))]);
        assert!(unknown.calls.is_empty() && unknown.errors.is_empty());
    }
}
