//! Спецтокены DeepSeek V3/R1 и V3.1: `<｜tool▁calls▁begin｜>…<｜tool▁calls▁end｜>`.
//! Веб может отдать их искажёнными, как DSML: черта `｜`/`|` любое число раз,
//! `▁` как пробел или `_`.

use super::{Family, Found, RawArgs, RawCall, json_at, skip_ws};
use regex::{Match, Regex};
use std::sync::LazyLock;

fn token(words: &str) -> String {
    format!(r"<[｜|]+\s*tool[▁ _]{words}\s*[｜|]+>")
}

static CALLS_BEGIN_RE: LazyLock<Regex> = LazyLock::new(|| re(&token("calls[▁ _]begin")));
static CALLS_END_RE: LazyLock<Regex> = LazyLock::new(|| re(&token("calls[▁ _]end")));
static CALL_BEGIN_RE: LazyLock<Regex> = LazyLock::new(|| re(&token("call[▁ _]begin")));
static CALL_END_RE: LazyLock<Regex> = LazyLock::new(|| re(&token("call[▁ _]end")));
static SEP_RE: LazyLock<Regex> = LazyLock::new(|| re(&token("sep")));
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^```[a-zA-Z]*[ \t]*\r?\n?"));

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("hardcoded regex is valid")
}

pub(super) fn families() -> Vec<Family> {
    vec![Family {
        opener: format!("{}|{}", token("calls[▁ _]begin"), token("call[▁ _]begin")),
        parse,
    }]
}

fn at_pos<'h>(re: &Regex, hay: &'h str, at: usize) -> Option<Match<'h>> {
    re.find_at(hay, at).filter(|m| m.start() == at)
}

fn parse(hay: &str, at: usize) -> Option<Found> {
    let Some(begin) = at_pos(&CALLS_BEGIN_RE, hay, at) else {
        return parse_call(hay, at);
    };
    // Секция: вызовы подряд до `calls▁end` или до конца текста.
    at_pos(&CALL_BEGIN_RE, hay, skip_ws(hay, begin.end()))?;
    let mut items = Vec::new();
    let mut pos = begin.end();
    loop {
        let p = skip_ws(hay, pos);
        if let Some(end) = at_pos(&CALLS_END_RE, hay, p) {
            pos = end.end();
            break;
        }
        if at_pos(&CALL_BEGIN_RE, hay, p).is_none() {
            break;
        }
        let found = parse_call(hay, p)?;
        pos = found.span.end;
        let broken = found.items.iter().any(Result::is_err);
        items.extend(found.items);
        if broken {
            pos = hay.len();
            break;
        }
    }
    Some(Found {
        span: at..pos,
        items,
    })
}

/// Один вызов: V3 — `function<sep>имя\n```json\n{…}\n```<call▁end>`,
/// V3.1 — `имя<sep>{…}<call▁end>`.
fn parse_call(hay: &str, at: usize) -> Option<Found> {
    let begin = at_pos(&CALL_BEGIN_RE, hay, at)?;
    let Some(sep) = SEP_RE.find_at(hay, begin.end()) else {
        return Some(broken(hay, at, "the call has no <｜tool▁sep｜> token"));
    };
    let head = hay[begin.end()..sep.start()].trim();
    let mut pos = skip_ws(hay, sep.end());
    // Step3: те же токены, внутри — `steptml:invoke` (sglang `step3_detector.py`).
    if hay[pos..].starts_with("<steptml:invoke") {
        return Some(step3_call(hay, at, pos));
    }
    let (name, format) = if hay[pos..].starts_with('{') || head != "function" {
        (head.to_string(), "deepseek_v31")
    } else {
        let line_end = hay[pos..].find('\n').map_or(hay.len(), |i| pos + i);
        let name = hay[pos..line_end].trim().to_string();
        pos = skip_ws(hay, line_end);
        if let Some(fence) = FENCE_RE.find(&hay[pos..]) {
            pos = skip_ws(hay, pos + fence.end());
        }
        (name, "deepseek_v3")
    };
    if name.is_empty() {
        return Some(broken(hay, at, "the call has no tool name"));
    }
    let (value, mut end) = match json_at(hay, pos) {
        Ok((value, end)) if value.is_object() => (value, end),
        Ok(_) => return Some(broken(hay, at, "the arguments are not a JSON object")),
        Err(error) => {
            return Some(broken(
                hay,
                at,
                &format!("the arguments are not valid JSON ({error})"),
            ));
        }
    };
    let after = skip_ws(hay, end);
    end = hay[after..].strip_prefix("```").map_or(end, |_| after + 3);
    let after = skip_ws(hay, end);
    let end = match at_pos(&CALL_END_RE, hay, after) {
        Some(close) => close.end(),
        None if after == hay.len() || at_pos(&CALLS_END_RE, hay, after).is_some() => end,
        None => return Some(broken(hay, at, "unexpected text after the arguments")),
    };
    Some(Found::call(
        at..end,
        RawCall {
            name,
            args: RawArgs::Json(value),
            format,
        },
    ))
}

fn step3_call(hay: &str, at: usize, invoke: usize) -> Found {
    let Some(found) = super::invoke::steptml_invoke(hay, invoke) else {
        return broken(hay, at, "the <steptml:invoke> tag could not be read");
    };
    if found.items.iter().any(Result::is_err) {
        return Found {
            span: at..hay.len(),
            items: found.items,
        };
    }
    let after = skip_ws(hay, found.span.end);
    let end = at_pos(&CALL_END_RE, hay, after).map_or(found.span.end, |close| close.end());
    Found {
        span: at..end,
        items: found.items,
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::parse_reply;

    /// Дословно из sglang `deepseekv3_detector.py`.
    #[test]
    fn v3_calls_parse() {
        let text = "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>get_current_weather\n```json\n{\"location\": \"Tokyo\"}\n```<｜tool▁call▁end｜>\n<｜tool▁call▁begin｜>function<｜tool▁sep｜>get_current_weather\n```json\n{\"location\": \"Paris\"}\n```<｜tool▁call▁end｜><｜tool▁calls▁end｜>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_current_weather");
        assert_eq!(reply.calls[1].arguments["location"], "Paris");
        assert_eq!(reply.visible, "");
    }

    /// Дословно из sglang `deepseekv31_detector.py`.
    #[test]
    fn v3_1_calls_parse() {
        let text = "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>get_current_weather<｜tool▁sep｜>{\"location\": \"Tokyo\"}<｜tool▁call▁end｜><｜tool▁call▁begin｜>get_current_weather<｜tool▁sep｜>{\"location\": \"Paris\"}<｜tool▁call▁end｜><｜tool▁calls▁end｜>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["location"], "Tokyo");
    }

    /// Искажение веба по образцу DSML: двойные черты, `▁` пробелом.
    #[test]
    fn mangled_tokens_parse() {
        let text = "Reading.\n<｜｜tool calls begin｜｜><｜｜tool call begin｜｜>read_file<｜｜tool sep｜｜>{\"path\": \"a\"}<｜｜tool call end｜｜><｜｜tool calls end｜｜>";
        let reply = parse_reply(text);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "read_file");
        assert_eq!(reply.visible, "Reading.");
    }

    #[test]
    fn a_call_without_its_section_parses() {
        let reply = parse_reply("<｜tool▁call▁begin｜>ls<｜tool▁sep｜>{}<｜tool▁call▁end｜>");
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
    }

    #[test]
    fn broken_json_is_reported() {
        let reply =
            parse_reply("<｜tool▁call▁begin｜>ls<｜tool▁sep｜>{\"a\": }<｜tool▁call▁end｜>");
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1);
    }
}

fn broken(hay: &str, at: usize, why: &str) -> Found {
    Found::broken(
        at..hay.len(),
        format!(
            "a native DeepSeek tool-call token block could not be parsed: {why}. Re-send the \
             call as <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
        ),
    )
}
