//! Форматы, где вызов задаёт получатель канала (`to=…`), а не отдельный тег:
//! Harmony (sglang `gpt_oss_detector.py`) и Muse Glimmer (sglang `muse_glimmer_detector.py`, vLLM `muse_glimmer_tool_parser.py`).

use super::{Declared, Family, Found, Param, RawArgs, RawCall, json_at, skip_ws};
use regex::{Captures, Regex};
use std::sync::LazyLock;

const CANONICAL: &str =
    "<tool_use><name>TOOL</name><arguments>{ valid JSON }</arguments></tool_use>";

fn captures_at<'h>(re: &Regex, hay: &'h str, at: usize) -> Option<Captures<'h>> {
    re.captures_at(hay, at)
        .filter(|caps| caps.get(0).is_some_and(|m| m.start() == at))
}

pub(super) fn families() -> Vec<Family> {
    let mut all = harmony::families();
    all.extend(muse_glimmer::families());
    all
}

/// Harmony (gpt-oss): `<|channel|>commentary to=functions.имя <|constrain|>json<|message|>{…}<|call|>`.
/// Другие каналы (`analysis`, `final`) под заголовок не подходят и остаются текстом.
mod harmony {
    use super::*;

    const CALL_TOKEN: &str = "<|call|>";
    const FUNCTIONS_PREFIX: &str = "functions.";

    /// Заголовок целиком до `<|message|>`, где начинаются аргументы. Каналы
    /// `analysis`/`final` не содержат слова `commentary` и тут не совпадают.
    static HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?:{})?{}commentary\s+to=(?P<to>[A-Za-z_][A-Za-z0-9_.\-]*)\s*{}json{}",
            regex::escape("<|start|>assistant"),
            regex::escape("<|channel|>"),
            regex::escape("<|constrain|>"),
            regex::escape("<|message|>"),
        ))
        .expect("hardcoded regex is valid")
    });

    pub(super) fn families() -> Vec<Family> {
        vec![Family {
            opener: HEADER_RE.as_str().to_string(),
            parse,
        }]
    }

    fn parse(hay: &str, at: usize) -> Option<Found> {
        let header = captures_at(&HEADER_RE, hay, at)?;
        let body_from = header.get(0).expect("group 0").end();
        let recipient = header.name("to").expect("named group `to`").as_str();
        let (value, json_end) = match json_at(hay, body_from) {
            Ok((value, end)) if value.is_object() => (value, end),
            Ok(_) => {
                return Some(Found::broken(
                    at..hay.len(),
                    format!(
                        "a Harmony commentary call's arguments are not a JSON object. \
                         Re-send the call as {CANONICAL}."
                    ),
                ));
            }
            Err(error) => {
                return Some(Found::broken(
                    at..hay.len(),
                    format!(
                        "a Harmony commentary call's arguments are not valid JSON \
                         ({error}). Re-send the call as {CANONICAL}."
                    ),
                ));
            }
        };
        let after_json = skip_ws(hay, json_end);
        // Обрыв сразу после целого JSON — как и у своих форматов, не ошибка.
        let end = if hay[after_json..].starts_with(CALL_TOKEN) {
            after_json + CALL_TOKEN.len()
        } else if after_json == hay.len() {
            json_end
        } else {
            return Some(Found::broken(
                at..hay.len(),
                format!(
                    "unexpected text after a Harmony commentary call's arguments. \
                     Re-send the call as {CANONICAL}."
                ),
            ));
        };
        let name = recipient
            .strip_prefix(FUNCTIONS_PREFIX)
            .unwrap_or(recipient)
            .to_string();
        Some(Found::call(
            at..end,
            RawCall {
                name,
                args: RawArgs::Json(value),
                format: "harmony",
            },
        ))
    }

    #[cfg(test)]
    mod tests {
        use crate::agent::tool_parser::parse_with;
        use serde_json::json;

        /// Подставлен в шаблон из докстринга sglang `gpt_oss_detector.py`:
        /// `<|channel|>commentary to={namespace.function}<|constrain|>json<|message|>{args}<|call|>`.
        #[test]
        fn a_commentary_call_parses() {
            let text = "<|start|>assistant<|channel|>commentary to=functions.get_weather <|constrain|>json<|message|>{\"city\": \"Paris\"}<|call|>";
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
            assert_eq!(reply.calls[0].name, "get_weather");
            assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
            assert!(reply.calls[0].origin.needs_person());
            assert_eq!(reply.visible, "");
        }

        /// Без ведущего `<|start|>assistant` — так тоже бывает.
        #[test]
        fn a_bare_channel_header_without_start_assistant_parses() {
            let text = "<|channel|>commentary to=get_weather<|constrain|>json<|message|>{\"city\": \"Tokyo\"}<|call|>";
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
            assert_eq!(reply.calls[0].name, "get_weather");
        }

        /// Обрыв сразу после целого JSON, без `<|call|>` — вызов, а не ошибка.
        #[test]
        fn a_call_cut_off_right_after_the_json_still_runs() {
            let text = "<|channel|>commentary to=functions.ls<|constrain|>json<|message|>{}";
            let reply = parse_with(text, &[("ls", json!({}))]);
            assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        }

        /// Заголовок процитирован без получателя — обычный текст.
        #[test]
        fn a_channel_marker_without_a_recipient_is_prose() {
            let text = "The header looks like <|channel|>commentary but names no recipient here.";
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert!(reply.calls.is_empty() && reply.errors.is_empty());
            assert_eq!(reply.visible, text);
        }

        /// Битый JSON после полного заголовка — диагностика, не догадка.
        #[test]
        fn malformed_json_is_reported() {
            let text = "<|channel|>commentary to=functions.ls<|constrain|>json<|message|>{\"a\": }<|call|>";
            let reply = parse_with(text, &[("ls", json!({}))]);
            assert!(reply.calls.is_empty());
            assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
        }
    }
}

/// Muse Glimmer: `<|start|>assistant to=<инструмент><|message|>` открывает канал
/// (`to=self`/`to=user` — не вызовы); тело — один или несколько `<atem:invoke>`.
mod muse_glimmer {
    use super::*;

    const EOM: &str = "<|eom|>";
    const EOT: &str = "<|eot|>";
    const START: &str = "<|start|>";
    const INVOKE_OPEN: &str = "<atem:invoke";
    const INVOKE_CLOSE: &str = "</atem:invoke>";

    static HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?:{}\s*assistant\s*)?to=(?P<to>[^\s<]+){}",
            regex::escape(START),
            regex::escape("<|message|>"),
        ))
        .expect("hardcoded regex is valid")
    });

    static INVOKE_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"<atem:invoke\b[^>]*?\bname="(?P<name>[^"]*)"[^>]*?>"#)
            .expect("hardcoded regex is valid")
    });

    static PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)<atem:parameter\b[^>]*?\bname="(?P<key>[^"]*)"[^>]*?>(?P<value>.*?)</atem:parameter>"#)
            .expect("hardcoded regex is valid")
    });

    pub(super) fn families() -> Vec<Family> {
        vec![Family {
            opener: HEADER_RE.as_str().to_string(),
            parse,
        }]
    }

    /// Канал `to=self`/`to=user` — рассуждение или финальный ответ, не вызов.
    fn is_tool_channel(recipient: &str) -> bool {
        !matches!(recipient, "self" | "user")
    }

    fn parse(hay: &str, at: usize) -> Option<Found> {
        let header = captures_at(&HEADER_RE, hay, at)?;
        let header_end = header.get(0).expect("group 0").end();
        let recipient = header.name("to").expect("named group `to`").as_str();
        if !is_tool_channel(recipient) {
            return None;
        }
        let body_bound = [EOM, EOT, START]
            .into_iter()
            .filter_map(|marker| hay[header_end..].find(marker))
            .min()
            .map_or(hay.len(), |offset| header_end + offset);
        // Канал на инструмент без единого <atem:invoke> — просто текст.
        let first = hay[header_end..body_bound].find(INVOKE_OPEN)?;
        let mut items = Vec::new();
        let mut pos = header_end + first;
        loop {
            match parse_invoke(hay, pos) {
                Ok((call, end)) => {
                    items.push(Ok(call));
                    pos = end;
                    match hay[pos..].find(INVOKE_OPEN) {
                        Some(offset) if pos + offset >= body_bound => break,
                        Some(offset) => pos += offset,
                        None => break,
                    }
                }
                Err(error) => {
                    items.push(Err(format!("{error}. Re-send the call as {CANONICAL}.")));
                    return Some(Found {
                        span: at..hay.len(),
                        items,
                    });
                }
            }
        }
        let end = if hay[pos..].starts_with(EOM) {
            pos + EOM.len()
        } else if hay[pos..].starts_with(EOT) {
            pos + EOT.len()
        } else {
            pos
        };
        Some(Found {
            span: at..end,
            items,
        })
    }

    fn parse_invoke(hay: &str, at: usize) -> Result<(RawCall, usize), String> {
        let open = captures_at(&INVOKE_NAME_RE, hay, at)
            .ok_or_else(|| "a malformed <atem:invoke> tag".to_string())?;
        let name = open.name("name").expect("named group `name`").as_str();
        let body_from = open.get(0).expect("group 0").end();
        let Some(close_offset) = hay[body_from..].find(INVOKE_CLOSE) else {
            return Err(format!(
                "tool `{name}`: the call was cut off before its closing tag"
            ));
        };
        let body = &hay[body_from..body_from + close_offset];
        let end = body_from + close_offset + INVOKE_CLOSE.len();
        let params = PARAM_RE
            .captures_iter(body)
            .map(|cap| Param {
                name: cap["key"].to_string(),
                text: cap["value"].trim().to_string(),
                declared: Declared::Json,
            })
            .collect();
        Ok((
            RawCall {
                name: collapse_doubled_namespace(name),
                args: RawArgs::Params(params),
                format: "muse_glimmer",
            },
            end,
        ))
    }

    /// Шаблон чата иногда удваивает пространство имён (`x.x`) — сворачиваем
    /// его в `x`. Другие имена с точкой (`x.y`) оставляем как написаны.
    fn collapse_doubled_namespace(name: &str) -> String {
        match name.split_once('.') {
            Some((head, tail)) if head == tail => head.to_string(),
            _ => name.to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use crate::agent::tool_parser::parse_with;
        use serde_json::json;

        /// По докстрингу vLLM `muse_glimmer_tool_parser.py`: шаблон чата
        /// удваивает имя без точки в `get_weather.get_weather`.
        #[test]
        fn a_muse_glimmer_call_parses_and_collapses_the_doubled_name() {
            let text = "<|start|>assistant to=get_weather.get_weather<|message|><atem:function_calls>\n<atem:invoke name=\"get_weather.get_weather\">\n<atem:parameter name=\"city\">Paris</atem:parameter>\n</atem:invoke>\n</atem:function_calls><|eom|>";
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
            assert_eq!(reply.calls[0].name, "get_weather");
            assert_eq!(reply.calls[0].arguments, json!({"city": "Paris"}));
            assert!(reply.calls[0].origin.needs_person());
        }

        /// `to=self` — рассуждение, не вызов.
        #[test]
        fn a_self_channel_is_not_a_call() {
            let text = "<|start|>assistant to=self<|message|>planning the next step<|eom|>";
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert!(reply.calls.is_empty() && reply.errors.is_empty());
        }

        /// `to=user` — финальный ответ, не вызов.
        #[test]
        fn a_user_channel_is_not_a_call() {
            let text = "<|start|>assistant to=user<|message|>Here is the answer.<|eot|>";
            let reply = parse_with(text, &[("get_weather", json!({}))]);
            assert!(reply.calls.is_empty() && reply.errors.is_empty());
        }

        /// Несколько `<atem:invoke>` в одном канале; значение-число берётся JSON-ом.
        #[test]
        fn several_invokes_in_one_channel_parse() {
            let text = "<|start|>assistant to=tools<|message|><atem:invoke name=\"a\"><atem:parameter name=\"n\">1</atem:parameter></atem:invoke><atem:invoke name=\"b\"><atem:parameter name=\"n\">2</atem:parameter></atem:invoke><|eom|>";
            let reply = parse_with(text, &[("a", json!({})), ("b", json!({}))]);
            assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
            assert_eq!(reply.calls[0].arguments, json!({"n": 1}));
            assert_eq!(reply.calls[1].arguments, json!({"n": 2}));
        }

        /// Заголовок процитирован без `<|message|>` следом — обычный текст.
        #[test]
        fn a_recipient_mention_without_a_message_body_is_prose() {
            let text =
                "Muse Glimmer routes calls through a to=tool recipient before atem:invoke tags.";
            let reply = parse_with(text, &[("tool", json!({}))]);
            assert!(reply.calls.is_empty() && reply.errors.is_empty());
            assert_eq!(reply.visible, text);
        }

        /// Оборванный `<atem:invoke>` без закрывающего тега — диагностика.
        #[test]
        fn a_cut_off_invoke_is_reported() {
            let text = "<|start|>assistant to=tools<|message|><atem:invoke name=\"a\"><atem:parameter name=\"n\">1</atem:parameter>";
            let reply = parse_with(text, &[("a", json!({}))]);
            assert!(reply.calls.is_empty());
            assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
        }
    }
}
