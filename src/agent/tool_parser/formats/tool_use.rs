//! Наш основной формат `<tool_use>` и старый `[TOOL:имя] {json}`.

use super::{Family, Found, RawArgs, RawCall, json_at, skip_ws};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const OPEN: &str = "<tool_use>";
const CLOSE: &str = "</tool_use>";
const ARGS_OPEN: &str = "<arguments>";
const ARGS_CLOSE: &str = "</arguments>";

static LEGACY_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[TOOL:([^\]\n]+)\]").expect("hardcoded regex is valid"));

pub(super) fn families() -> Vec<Family> {
    vec![
        Family {
            opener: regex::escape(OPEN),
            parse: parse_tool_use,
        },
        Family {
            opener: r"\[TOOL:[^\]\n]+\]".to_string(),
            parse: parse_legacy,
        },
    ]
}

/// Допуски сверх строгой формы: перепутанные `</arguments>`/`</tool_use>`,
/// нет обёртки `<arguments>`, голый JSON `{"name"|"tool", "arguments"|"args"}`.
fn parse_tool_use(hay: &str, at: usize) -> Option<Found> {
    let inner = skip_ws(hay, at + OPEN.len());
    let rest = &hay[inner..];
    if let Some(after) = rest.strip_prefix("<name>") {
        let Some(len) = after.find("</name>") else {
            return Some(Found::broken(
                at..hay.len(),
                "a <tool_use> block was cut off inside <name>. Re-send the whole call.".into(),
            ));
        };
        let name = after[..len].trim().to_string();
        let args_from = inner + "<name>".len() + len + "</name>".len();
        return Some(named_call(hay, at, name, args_from));
    }
    rest.starts_with('{')
        .then(|| bare_json_call(hay, at, inner))
}

fn named_call(hay: &str, at: usize, name: String, from: usize) -> Found {
    let mut pos = skip_ws(hay, from);
    if hay[pos..].starts_with(ARGS_OPEN) {
        pos = skip_ws(hay, pos + ARGS_OPEN.len());
    } else {
        // Без обёртки — первый объект до конца вызова.
        let limit = hay[pos..].find(CLOSE).map_or(hay.len(), |i| pos + i);
        match hay[pos..limit].find('{') {
            Some(offset) => pos += offset,
            None => {
                return Found::broken(
                    at..close_end(hay, pos),
                    format!(
                        "tool `{name}`: missing <arguments> block. Send exactly \
                         <tool_use><name>{name}</name><arguments>{{ ... }}</arguments></tool_use>."
                    ),
                );
            }
        }
    }
    match json_at(hay, pos) {
        Ok((value, json_end)) => match closers_end(hay, json_end) {
            Some(end) => Found::call(
                at..end,
                RawCall {
                    name,
                    args: RawArgs::Json(value),
                    format: "tool_use",
                },
            ),
            None => Found::broken(
                at..close_end(hay, json_end),
                format!(
                    "tool `{name}`: unexpected text after the <arguments> JSON. Send exactly \
                     <tool_use><name>{name}</name><arguments>{{ ... }}</arguments></tool_use>."
                ),
            ),
        },
        Err(error) => Found::broken(
            at..close_end(hay, pos),
            format!(
                "tool `{name}`: <arguments> is not valid JSON ({error}). Re-send \
                 with a valid JSON object and escape every backslash (\\\\) and \
                 quote (\\\") inside string values."
            ),
        ),
    }
}

fn bare_json_call(hay: &str, at: usize, pos: usize) -> Found {
    let (value, json_end) = match json_at(hay, pos) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Found::broken(
                at..close_end(hay, pos),
                format!(
                    "malformed <tool_use> block ({error}). Use \
                     <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
                ),
            );
        }
    };
    let end = closers_end(hay, json_end).unwrap_or_else(|| close_end(hay, json_end));
    let name = value
        .get("tool")
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let Some(name) = name else {
        return Found::broken(
            at..end,
            "a <tool_use> block had no <name> tag and no \"tool\"/\"name\" field in its JSON."
                .into(),
        );
    };
    let args = value
        .get("args")
        .or_else(|| value.get("arguments"))
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    Found::call(
        at..end,
        RawCall {
            name,
            args: RawArgs::Json(args),
            format: "tool_use",
        },
    )
}

/// Конец закрывающих тегов после JSON: `</arguments>` и `</tool_use>` в
/// любом порядке, или конец текста. `None` — после JSON посторонний текст.
fn closers_end(hay: &str, from: usize) -> Option<usize> {
    closers_after(hay, from).or_else(|| {
        // Лишние `}` за целым объектом: так модель дописывает длинный вызов
        // по кускам (живой прогон 2026-09-29). На аргументы они не влияют.
        let stray = from
            + hay[from..]
                .find(|c: char| c != '}' && !c.is_whitespace())
                .unwrap_or(hay.len() - from);
        (hay[from..stray].contains('}'))
            .then(|| closers_after(hay, stray))
            .flatten()
    })
}

fn closers_after(hay: &str, from: usize) -> Option<usize> {
    let mut end = from;
    let mut seen_args = false;
    let mut seen_close = false;
    loop {
        let pos = skip_ws(hay, end);
        let rest = &hay[pos..];
        if !seen_close && rest.starts_with(CLOSE) {
            seen_close = true;
            end = pos + CLOSE.len();
        } else if !seen_args && rest.starts_with(ARGS_CLOSE) {
            seen_args = true;
            end = pos + ARGS_CLOSE.len();
        } else {
            // Поток оборвался сразу за JSON — вызов целый.
            return (seen_close || seen_args || pos == hay.len()).then_some(end);
        }
    }
}

/// Конец битого вызова: за первым `</tool_use>`, а без него — конец текста.
fn close_end(hay: &str, from: usize) -> usize {
    hay[from..]
        .find(CLOSE)
        .map_or(hay.len(), |i| from + i + CLOSE.len())
}

fn parse_legacy(hay: &str, at: usize) -> Option<Found> {
    let open = LEGACY_OPEN_RE.captures(&hay[at..])?;
    let name = open[1].to_string();
    let pos = skip_ws(hay, at + open[0].len());
    if !hay[pos..].starts_with('{') {
        return None;
    }
    Some(match json_at(hay, pos) {
        Ok((value, end)) => Found::call(
            at..end,
            RawCall {
                name,
                args: RawArgs::Json(value),
                format: "legacy",
            },
        ),
        Err(error) => Found::broken(
            at..hay.len(),
            format!("tool `{name}`: arguments are not valid JSON ({error})."),
        ),
    })
}
