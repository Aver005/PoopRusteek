//! Семейство «ключ-значение» (`<arg_key>`/`<arg_value>`: GLM 4.5/4.7, Ling3,
//! Spark2.5, Poolside v1, Hunyuan/Hy3/Hy4, K2 Horizon) и семейство «function=»
//! (Qwen3-Coder, Seed-OSS, MiMo, Step3.5, MiniCPM5). Маркеры `<tool_call>` и
//! `<tool_calls>` общие с JSON и `<invoke>` — выбор идёт по телу.

use super::{Declared, Family, Found, Param, RawArgs, RawCall, json_at, skip_ws};
use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::LazyLock;

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";
const SEED_OPEN: &str = "<seed:tool_call>";
const SEED_CLOSE: &str = "</seed:tool_call>";

const GLM_ARG_KEY_OPEN: &str = "<arg_key>";
const GLM_ARG_KEY_CLOSE: &str = "</arg_key>";
const GLM_ARG_VALUE_OPEN: &str = "<arg_value>";
const GLM_ARG_VALUE_CLOSE: &str = "</arg_value>";

const FUNCTION_CLOSE: &str = "</function>";
const PARAMETER_CLOSE: &str = "</parameter>";
const MINICPM5_PARAM_CLOSE: &str = "</param>";

const K2_CALLS_OPEN: &str = "<ifm|tool_calls>";
const K2_CALLS_CLOSE: &str = "</ifm|tool_calls>";
const K2_CALL_OPEN: &str = "<ifm|tool_call>";
const K2_CALL_CLOSE: &str = "</ifm|tool_call>";
const K2_ARG_KEY_OPEN: &str = "<ifm|arg_key>";
const K2_ARG_KEY_CLOSE: &str = "</ifm|arg_key>";
const K2_ARG_TYPE_OPEN: &str = "<ifm|arg_type>";
const K2_ARG_TYPE_CLOSE: &str = "</ifm|arg_type>";
const K2_ARG_VALUE_OPEN: &str = "<ifm|arg_value>";
const K2_ARG_VALUE_CLOSE: &str = "</ifm|arg_value>";

// `<function=имя>`/`<function имя>` (Step3.5 чинит форму без `=`); имя без
// `=` ограничено идентификатором, чтобы не поймать MiniCPM5 `<function
// name="x">`.
static FUNCTION_OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<function(?:=([^>]*)|[ \t]+([A-Za-z_][\w.\-]*))[ \t]*>")
        .expect("hardcoded regex is valid")
});
static PARAMETER_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<parameter=([^>]*)>").expect("hardcoded regex is valid"));

static MINICPM5_FUNC_OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<function\s+name\s*=\s*(?:"([^"]*)"|'([^']*)')[^>]*>"#)
        .expect("hardcoded regex is valid")
});
static MINICPM5_PARAM_OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<param\s+name\s*=\s*(?:"([^"]*)"|'([^']*)')\s*>"#)
        .expect("hardcoded regex is valid")
});

// Hunyuan/Hy3/Hy4: теги бывают с суффиксом (`<tool_call:opensource>`).
static HY_CALLS_OPEN: LazyLock<Regex> = LazyLock::new(|| hy_tag("tool_calls", false));
static HY_CALLS_CLOSE: LazyLock<Regex> = LazyLock::new(|| hy_tag("tool_calls", true));
static HY_CALL_OPEN: LazyLock<Regex> = LazyLock::new(|| hy_tag("tool_call", false));
static HY_CALL_CLOSE: LazyLock<Regex> = LazyLock::new(|| hy_tag("tool_call", true));
static HY_SEP: LazyLock<Regex> = LazyLock::new(|| hy_tag("tool_sep", false));
static HY_ARG_KEY_OPEN: LazyLock<Regex> = LazyLock::new(|| hy_tag("arg_key", false));
static HY_ARG_KEY_CLOSE: LazyLock<Regex> = LazyLock::new(|| hy_tag("arg_key", true));
static HY_ARG_VALUE_OPEN: LazyLock<Regex> = LazyLock::new(|| hy_tag("arg_value", false));
static HY_ARG_VALUE_CLOSE: LazyLock<Regex> = LazyLock::new(|| hy_tag("arg_value", true));

static ENTITY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(lt|gt|amp|quot|apos|#[0-9]+|#[xX][0-9A-Fa-f]+);")
        .expect("hardcoded regex is valid")
});

fn hy_tag(name: &str, close: bool) -> Regex {
    let slash = if close { "/" } else { "" };
    Regex::new(&format!(r"<{slash}{name}(?::[^>\s]+)?>")).expect("hardcoded regex is valid")
}

pub(super) fn families() -> Vec<Family> {
    vec![
        Family {
            opener: regex::escape(TOOL_CALL_OPEN),
            parse: parse_tool_call,
        },
        Family {
            opener: regex::escape(SEED_OPEN),
            parse: parse_seed_oss,
        },
        Family {
            opener: r"<function(?:=|[ \t]+[A-Za-z_])".to_string(),
            parse: parse_bare_function,
        },
        Family {
            opener: r#"<function\s+name\s*=\s*(?:"|')"#.to_string(),
            parse: parse_minicpm5,
        },
        Family {
            opener: r"<tool_calls(?::[^>\s]+)?>".to_string(),
            parse: parse_hunyuan,
        },
        Family {
            opener: regex::escape(K2_CALLS_OPEN),
            parse: parse_k2_group,
        },
        Family {
            opener: regex::escape(K2_CALL_OPEN),
            parse: parse_k2_call,
        },
    ]
}

/// Совпадение ровно на позиции `at`.
fn at_pos<'h>(re: &Regex, hay: &'h str, at: usize) -> Option<Captures<'h>> {
    re.captures_at(hay, at)
        .filter(|caps| caps.get(0).is_some_and(|m| m.start() == at))
}

fn first_group(caps: &Captures, groups: std::ops::RangeInclusive<usize>) -> String {
    groups
        .filter_map(|i| caps.get(i))
        .map(|m| m.as_str().to_string())
        .next()
        .unwrap_or_default()
}

/// Имя вызова из свободного текста: буква/`_` первым символом, дальше
/// идентификаторные символы. Не идентификатор — скорее проза, а не вызов.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

fn cut_off(name: &str, tag: &str) -> String {
    format!(
        "tool `{name}`: a {tag} block was cut off before its closing tag. Re-send the call as \
         <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
    )
}

fn malformed(name: &str, tag: &str) -> String {
    format!("tool `{name}`: a malformed {tag} tag inside the call. Re-send it.")
}

/// `&lt; &gt; &amp; &quot; &apos;`/`&#39;` и числовые сущности (`html.unescape`
/// у MiMo, сущности XML у MiniCPM5).
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    ENTITY_RE
        .replace_all(text, |caps: &Captures| entity_char(&caps[1]))
        .into_owned()
}

fn entity_char(name: &str) -> String {
    let code = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        u32::from_str_radix(hex, 16).ok()
    } else if let Some(dec) = name.strip_prefix('#') {
        dec.parse::<u32>().ok()
    } else {
        return match name {
            "lt" => "<".to_string(),
            "gt" => ">".to_string(),
            "amp" => "&".to_string(),
            "quot" => "\"".to_string(),
            "apos" => "'".to_string(),
            _ => format!("&{name};"),
        };
    };
    code.and_then(char::from_u32)
        .map(String::from)
        .unwrap_or_else(|| format!("&{name};"))
}

// ---------------------------------------------------------------------
// Общий маркер `<tool_call>`: JSON/invoke — не наше, иначе function= или
// arg_key/arg_value по телу.
// ---------------------------------------------------------------------

fn parse_tool_call(hay: &str, at: usize) -> Option<Found> {
    let body = skip_ws(hay, at + TOOL_CALL_OPEN.len());
    let rest = &hay[body..];
    if rest.starts_with('{') || rest.starts_with('[') || rest.starts_with("<invoke") {
        return None;
    }
    qwen_call(hay, at, body, Some(TOOL_CALL_CLOSE))
        .or_else(|| glm_call(hay, at, body, TOOL_CALL_CLOSE))
}

fn parse_seed_oss(hay: &str, at: usize) -> Option<Found> {
    let body = skip_ws(hay, at + SEED_OPEN.len());
    qwen_call(hay, at, body, Some(SEED_CLOSE))
}

/// `<function=имя>`/`<function имя>` без обёртки `<tool_call>` (sglang
/// #40236 и Step3.5's engine wrapper).
fn parse_bare_function(hay: &str, at: usize) -> Option<Found> {
    qwen_call(hay, at, at, None)
}

// ---------------------------------------------------------------------
// GLM 4.5/4.7, Ling3, Spark2.5, Poolside v1: `<tool_call>имя<arg_key>k
// </arg_key><arg_value>v</arg_value>…</tool_call>`. Дословно в тексте
// неразличимы — одна грамматика, тег "glm".
// ---------------------------------------------------------------------

fn glm_call(hay: &str, at: usize, body: usize, close: &str) -> Option<Found> {
    let rest = &hay[body..];
    let arg_key = rest.find(GLM_ARG_KEY_OPEN).map(|i| body + i);
    let end = rest.find(close).map(|i| body + i);
    let name_end = [arg_key, end].into_iter().flatten().min()?;
    let name = hay[body..name_end].trim();
    if !is_identifier(name) {
        return None;
    }
    let name = name.to_string();
    let mut pos = name_end;
    let mut params = Vec::new();
    loop {
        if hay[pos..].starts_with(close) {
            return Some(Found::call(
                at..pos + close.len(),
                RawCall {
                    name,
                    args: RawArgs::Params(params),
                    format: "glm",
                },
            ));
        }
        if hay[pos..].starts_with(GLM_ARG_KEY_OPEN) {
            let key_from = pos + GLM_ARG_KEY_OPEN.len();
            let Some(key_len) = hay[key_from..].find(GLM_ARG_KEY_CLOSE) else {
                return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_call>")));
            };
            let key = hay[key_from..key_from + key_len].trim().to_string();
            let vpos = skip_glm_sep(hay, key_from + key_len + GLM_ARG_KEY_CLOSE.len());
            if !hay[vpos..].starts_with(GLM_ARG_VALUE_OPEN) {
                return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_call>")));
            }
            let v_from = vpos + GLM_ARG_VALUE_OPEN.len();
            let Some(v_len) = hay[v_from..].find(GLM_ARG_VALUE_CLOSE) else {
                return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_call>")));
            };
            params.push(Param {
                name: key,
                text: hay[v_from..v_from + v_len].to_string(),
                declared: Declared::BySchema,
            });
            pos = v_from + v_len + GLM_ARG_VALUE_CLOSE.len();
            continue;
        }
        let next = [
            hay[pos..].find(GLM_ARG_KEY_OPEN).map(|i| pos + i),
            hay[pos..].find(close).map(|i| pos + i),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next) = next else {
            return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_call>")));
        };
        if hay[pos..next].contains('<') {
            return Some(Found::broken(
                at..hay.len(),
                malformed(&name, "<arg_key>/<arg_value>"),
            ));
        }
        pos = next;
    }
}

/// Разделитель `</arg_key>`↔`<arg_value>`: буквальный `\n` или пробел, любое
/// число раз (sglang `(?:\\n|\s)*`).
fn skip_glm_sep(hay: &str, mut pos: usize) -> usize {
    loop {
        if hay[pos..].starts_with("\\n") {
            pos += 2;
            continue;
        }
        if let Some(c) = hay[pos..].chars().next()
            && c.is_whitespace()
        {
            pos += c.len_utf8();
            continue;
        }
        return pos;
    }
}

// ---------------------------------------------------------------------
// Qwen3-Coder/Seed-OSS/MiMo/Step3.5/Nemotron: `<function=имя><parameter=k>
// v</parameter></function>`. MiMo дополнительно раскодирует сущности —
// применяем всегда, для остальных это ничего не меняет.
// ---------------------------------------------------------------------

fn qwen_call(hay: &str, at: usize, body: usize, wrap_close: Option<&str>) -> Option<Found> {
    let open = at_pos(&FUNCTION_OPEN_RE, hay, body)?;
    let name = first_group(&open, 1..=2);
    let name = name.trim();
    if !is_identifier(name) {
        return None;
    }
    let name = name.to_string();
    let mut pos = open.get(0).expect("group 0").end();
    let mut params = Vec::new();
    loop {
        if hay[pos..].starts_with(FUNCTION_CLOSE) {
            let end = pos + FUNCTION_CLOSE.len();
            let end = match wrap_close {
                Some(close) => {
                    let p = skip_ws(hay, end);
                    if hay[p..].starts_with(close) {
                        p + close.len()
                    } else {
                        end
                    }
                }
                None => end,
            };
            return Some(Found::call(
                at..end,
                RawCall {
                    name,
                    args: RawArgs::Params(params),
                    format: "qwen3_coder",
                },
            ));
        }
        if let Some(popen) = at_pos(&PARAMETER_OPEN_RE, hay, pos) {
            let pname = popen[1].trim().to_string();
            let value_from = popen.get(0).expect("group 0").end();
            let Some((value_end, next)) = qwen_value_end(hay, value_from) else {
                return Some(Found::broken(
                    at..hay.len(),
                    cut_off(&name, "<function=...>"),
                ));
            };
            let raw = trim_one_newline(&hay[value_from..value_end]);
            params.push(Param {
                name: pname,
                text: decode_entities(raw),
                declared: Declared::BySchema,
            });
            pos = next;
            continue;
        }
        let next_tag = [
            PARAMETER_OPEN_RE.find_at(hay, pos).map(|m| m.start()),
            hay[pos..].find(FUNCTION_CLOSE).map(|i| pos + i),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next_tag) = next_tag else {
            return Some(Found::broken(
                at..hay.len(),
                cut_off(&name, "<function=...>"),
            ));
        };
        if hay[pos..next_tag].contains('<') {
            return Some(Found::broken(
                at..hay.len(),
                malformed(&name, "<parameter=>"),
            ));
        }
        pos = next_tag;
    }
}

/// Конец значения параметра: `</parameter>` (съедается) или лукахед на
/// следующий `<parameter=`/`</function>` — незакрытый кончается там же.
fn qwen_value_end(hay: &str, from: usize) -> Option<(usize, usize)> {
    let close = hay[from..].find(PARAMETER_CLOSE).map(|i| (from + i, true));
    let next_param = PARAMETER_OPEN_RE
        .find_at(hay, from)
        .map(|m| (m.start(), false));
    let func_close = hay[from..].find(FUNCTION_CLOSE).map(|i| (from + i, false));
    let (pos, consume) = [close, next_param, func_close]
        .into_iter()
        .flatten()
        .min_by_key(|(p, _)| *p)?;
    Some(if consume {
        (pos, pos + PARAMETER_CLOSE.len())
    } else {
        (pos, pos)
    })
}

fn trim_one_newline(text: &str) -> &str {
    let text = text.strip_prefix('\n').unwrap_or(text);
    text.strip_suffix('\n').unwrap_or(text)
}

// ---------------------------------------------------------------------
// MiniCPM5: `<function name="x"><param name="k">v</param></function>`,
// CDATA и сущности XML, значения strip.
// ---------------------------------------------------------------------

fn parse_minicpm5(hay: &str, at: usize) -> Option<Found> {
    let open = at_pos(&MINICPM5_FUNC_OPEN_RE, hay, at)?;
    let name = first_group(&open, 1..=2);
    if name.is_empty() {
        return None;
    }
    let mut pos = open.get(0).expect("group 0").end();
    let mut params = Vec::new();
    loop {
        let p = skip_ws(hay, pos);
        if hay[p..].starts_with(FUNCTION_CLOSE) {
            return Some(Found::call(
                at..p + FUNCTION_CLOSE.len(),
                RawCall {
                    name,
                    args: RawArgs::Params(params),
                    format: "minicpm5",
                },
            ));
        }
        if let Some(popen) = at_pos(&MINICPM5_PARAM_OPEN_RE, hay, p) {
            let pname = first_group(&popen, 1..=2);
            let vfrom = popen.get(0).expect("group 0").end();
            let Some(vend) = hay[vfrom..].find(MINICPM5_PARAM_CLOSE).map(|i| vfrom + i) else {
                return Some(Found::broken(
                    at..hay.len(),
                    cut_off(&name, "<function name=...>"),
                ));
            };
            params.push(Param {
                name: pname,
                text: minicpm5_value(&hay[vfrom..vend]),
                declared: Declared::BySchema,
            });
            pos = vend + MINICPM5_PARAM_CLOSE.len();
            continue;
        }
        let next_tag = [
            MINICPM5_PARAM_OPEN_RE.find_at(hay, p).map(|m| m.start()),
            hay[p..].find(FUNCTION_CLOSE).map(|i| p + i),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next_tag) = next_tag else {
            return Some(Found::broken(
                at..hay.len(),
                cut_off(&name, "<function name=...>"),
            ));
        };
        if hay[p..next_tag].contains('<') {
            return Some(Found::broken(at..hay.len(), malformed(&name, "<param>")));
        }
        pos = next_tag;
    }
}

/// CDATA сохраняется дословно (без раскодирования сущностей внутри неё, как
/// у настоящего XML-парсера); иначе — сущности, затем пробелы по краям.
fn minicpm5_value(raw: &str) -> String {
    let text = raw.trim();
    if let Some(cdata) = text
        .strip_prefix("<![CDATA[")
        .and_then(|t| t.strip_suffix("]]>"))
    {
        return cdata.to_string();
    }
    decode_entities(text)
}

// ---------------------------------------------------------------------
// Hunyuan/Hy3/Hy4: `<tool_calls><tool_call>имя<tool_sep><arg_key>…`. Hy4 не
// шлёт `<tool_sep>` — граница имени тогда сразу на `<arg_key>`/закрытии.
// ---------------------------------------------------------------------

fn parse_hunyuan(hay: &str, at: usize) -> Option<Found> {
    let open = at_pos(&HY_CALLS_OPEN, hay, at)?;
    let from = skip_ws(hay, open.get(0).expect("group 0").end());
    at_pos(&HY_CALL_OPEN, hay, from)?;
    let mut items = Vec::new();
    let mut pos = from;
    loop {
        let p = skip_ws(hay, pos);
        if let Some(close) = at_pos(&HY_CALLS_CLOSE, hay, p) {
            return Some(Found {
                span: at..close.get(0).expect("group 0").end(),
                items,
            });
        }
        let Some(call_open) = at_pos(&HY_CALL_OPEN, hay, p) else {
            break;
        };
        let call_span = call_open.get(0).expect("group 0");
        let Some(found) = hunyuan_call(hay, call_span.start(), call_span.end()) else {
            break;
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

fn hunyuan_call(hay: &str, at: usize, from: usize) -> Option<Found> {
    let sep = HY_SEP.find_at(hay, from).map(|m| (m.start(), m.end()));
    let arg = HY_ARG_KEY_OPEN.find_at(hay, from).map(|m| m.start());
    let end = HY_CALL_CLOSE.find_at(hay, from).map(|m| m.start());
    let name_end = [sep.map(|(s, _)| s), arg, end]
        .into_iter()
        .flatten()
        .min()?;
    let args_from = match sep {
        Some((s, e)) if s == name_end => e,
        _ => name_end,
    };
    let name = hay[from..name_end].trim();
    if !is_identifier(name) {
        return None;
    }
    let name = name.to_string();
    let mut pos = args_from;
    let mut params = Vec::new();
    loop {
        if let Some(close) = at_pos(&HY_CALL_CLOSE, hay, pos) {
            return Some(Found::call(
                at..close.get(0).expect("group 0").end(),
                RawCall {
                    name,
                    args: RawArgs::Params(params),
                    format: "hunyuan",
                },
            ));
        }
        if let Some(key_open) = at_pos(&HY_ARG_KEY_OPEN, hay, pos) {
            let key_from = key_open.get(0).expect("group 0").end();
            let Some(key_close) = HY_ARG_KEY_CLOSE.find_at(hay, key_from) else {
                return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_calls>")));
            };
            let key = hay[key_from..key_close.start()].trim().to_string();
            let vpos = skip_ws(hay, key_close.end());
            let Some(val_open) = at_pos(&HY_ARG_VALUE_OPEN, hay, vpos) else {
                return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_calls>")));
            };
            let val_from = val_open.get(0).expect("group 0").end();
            let Some(val_close) = HY_ARG_VALUE_CLOSE.find_at(hay, val_from) else {
                return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_calls>")));
            };
            params.push(Param {
                name: key,
                text: hay[val_from..val_close.start()].to_string(),
                declared: Declared::BySchema,
            });
            pos = val_close.end();
            continue;
        }
        let next = [
            HY_ARG_KEY_OPEN.find_at(hay, pos).map(|m| m.start()),
            HY_CALL_CLOSE.find_at(hay, pos).map(|m| m.start()),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next) = next else {
            return Some(Found::broken(at..hay.len(), cut_off(&name, "<tool_calls>")));
        };
        if hay[pos..next].contains('<') {
            return Some(Found::broken(
                at..hay.len(),
                malformed(&name, "<arg_key>/<arg_value>"),
            ));
        }
        pos = next;
    }
}

// ---------------------------------------------------------------------
// K2 Horizon: `<ifm|tool_call>` — тело XML (`<ifm|arg_key>`, необязательный
// `<ifm|arg_type>`, `<ifm|arg_value>`) или JSON, снаружи необязательная
// группа `<ifm|tool_calls>`.
// ---------------------------------------------------------------------

fn parse_k2_group(hay: &str, at: usize) -> Option<Found> {
    let from = skip_ws(hay, at + K2_CALLS_OPEN.len());
    if !hay[from..].starts_with(K2_CALL_OPEN) {
        return None;
    }
    let mut items = Vec::new();
    let mut pos = from;
    loop {
        let p = skip_ws(hay, pos);
        if hay[p..].starts_with(K2_CALLS_CLOSE) {
            return Some(Found {
                span: at..p + K2_CALLS_CLOSE.len(),
                items,
            });
        }
        if !hay[p..].starts_with(K2_CALL_OPEN) {
            break;
        }
        let Some(found) = parse_k2_call(hay, p) else {
            break;
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

fn parse_k2_call(hay: &str, at: usize) -> Option<Found> {
    let body = skip_ws(hay, at + K2_CALL_OPEN.len());
    if hay[body..].starts_with('{') || hay[body..].starts_with('[') {
        return k2_json_block(hay, at, body);
    }
    k2_xml_block(hay, at, body)
}

fn k2_json_block(hay: &str, at: usize, pos: usize) -> Option<Found> {
    let (value, end) = json_at(hay, pos).ok()?;
    let close = skip_ws(hay, end);
    if !hay[close..].starts_with(K2_CALL_CLOSE) {
        return Some(Found::broken(
            at..hay.len(),
            "an <ifm|tool_call> JSON body has trailing text before its closing tag. Re-send it."
                .into(),
        ));
    }
    let span_end = close + K2_CALL_CLOSE.len();
    let raw_calls = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    let mut items = Vec::new();
    for raw in raw_calls {
        let func = raw.get("function").cloned().unwrap_or(raw);
        let name = func.get("name").and_then(Value::as_str).map(str::to_string);
        match name {
            Some(name) => {
                let args = func
                    .get("arguments")
                    .or_else(|| func.get("parameters"))
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                items.push(Ok(RawCall {
                    name,
                    args: RawArgs::Json(args),
                    format: "k2_horizon",
                }));
            }
            None => items.push(Err(
                "an <ifm|tool_call> JSON object is missing a function name. Re-send it.".into(),
            )),
        }
    }
    Some(Found {
        span: at..span_end,
        items,
    })
}

fn k2_xml_block(hay: &str, at: usize, body: usize) -> Option<Found> {
    let arg = hay[body..].find(K2_ARG_KEY_OPEN).map(|i| body + i);
    let close = hay[body..].find(K2_CALL_CLOSE).map(|i| body + i);
    let name_end = [arg, close].into_iter().flatten().min()?;
    let name = hay[body..name_end].trim();
    if !is_identifier(name) {
        return None;
    }
    let name = name.to_string();
    let mut pos = name_end;
    let mut params = Vec::new();
    loop {
        if hay[pos..].starts_with(K2_CALL_CLOSE) {
            return Some(Found::call(
                at..pos + K2_CALL_CLOSE.len(),
                RawCall {
                    name,
                    args: RawArgs::Params(params),
                    format: "k2_horizon",
                },
            ));
        }
        if hay[pos..].starts_with(K2_ARG_KEY_OPEN) {
            let key_from = pos + K2_ARG_KEY_OPEN.len();
            let Some(key_len) = hay[key_from..].find(K2_ARG_KEY_CLOSE) else {
                return Some(Found::broken(
                    at..hay.len(),
                    cut_off(&name, "<ifm|tool_call>"),
                ));
            };
            let key = hay[key_from..key_from + key_len].trim().to_string();
            let mut vpos = skip_ws(hay, key_from + key_len + K2_ARG_KEY_CLOSE.len());
            let mut declared_type = None;
            if hay[vpos..].starts_with(K2_ARG_TYPE_OPEN) {
                let t_from = vpos + K2_ARG_TYPE_OPEN.len();
                let Some(t_len) = hay[t_from..].find(K2_ARG_TYPE_CLOSE) else {
                    return Some(Found::broken(
                        at..hay.len(),
                        cut_off(&name, "<ifm|tool_call>"),
                    ));
                };
                declared_type = Some(hay[t_from..t_from + t_len].trim().to_string());
                vpos = skip_ws(hay, t_from + t_len + K2_ARG_TYPE_CLOSE.len());
            }
            if !hay[vpos..].starts_with(K2_ARG_VALUE_OPEN) {
                return Some(Found::broken(
                    at..hay.len(),
                    cut_off(&name, "<ifm|tool_call>"),
                ));
            }
            let v_from = vpos + K2_ARG_VALUE_OPEN.len();
            let Some(v_len) = hay[v_from..].find(K2_ARG_VALUE_CLOSE) else {
                return Some(Found::broken(
                    at..hay.len(),
                    cut_off(&name, "<ifm|tool_call>"),
                ));
            };
            let declared = match declared_type.as_deref() {
                Some(t) if t.eq_ignore_ascii_case("string") => Declared::Text,
                Some(_) => Declared::Json,
                None => Declared::BySchema,
            };
            params.push(Param {
                name: key,
                text: hay[v_from..v_from + v_len].to_string(),
                declared,
            });
            pos = v_from + v_len + K2_ARG_VALUE_CLOSE.len();
            continue;
        }
        let next = [
            hay[pos..].find(K2_ARG_KEY_OPEN).map(|i| pos + i),
            hay[pos..].find(K2_CALL_CLOSE).map(|i| pos + i),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next) = next else {
            return Some(Found::broken(
                at..hay.len(),
                cut_off(&name, "<ifm|tool_call>"),
            ));
        };
        if hay[pos..next].contains('<') {
            return Some(Found::broken(
                at..hay.len(),
                malformed(&name, "<ifm|arg_key>/<ifm|arg_value>"),
            ));
        }
        pos = next;
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tool_parser::parse_with;
    use serde_json::json;

    fn weather() -> Vec<(&'static str, serde_json::Value)> {
        vec![(
            "get_weather",
            json!({"type": "object", "properties": {
                "city": {"type": "string"}, "date": {"type": "string"}
            }}),
        )]
    }

    /// Дословно из sglang `glm4_moe_detector.py` (docstring, настоящие
    /// переводы строк).
    #[test]
    fn glm45_reference_example_parses() {
        let text = "<tool_call>get_weather\n<arg_key>city</arg_key>\n<arg_value>北京</arg_value>\n<arg_key>date</arg_key>\n<arg_value>2024-06-27</arg_value>\n</tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "北京", "date": "2024-06-27"})
        );
        assert!(reply.calls[0].origin.needs_person());
        assert_eq!(reply.visible, "");
    }

    /// Дословно из sglang `glm47_moe_detector.py` (docstring): без переводов
    /// строк, два вызова подряд без разделителя.
    #[test]
    fn glm47_two_calls_without_separator_parse() {
        let text = "<tool_call>get_weather<arg_key>city</arg_key><arg_value>北京</arg_value><arg_key>date</arg_key><arg_value>2024-06-27</arg_value></tool_call><tool_call>get_weather<arg_key>city</arg_key><arg_value>上海</arg_value><arg_key>date</arg_key><arg_value>2024-06-27</arg_value></tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "北京");
        assert_eq!(reply.calls[1].arguments["city"], "上海");
    }

    /// Ling3 (sglang `ling3_detector.py`): вызов без аргументов закрывается
    /// сразу, без `<arg_key>`.
    #[test]
    fn ling3_call_without_arguments_closes_immediately() {
        let reply = parse_with(
            "<tool_call>list_tasks</tool_call>",
            &[("list_tasks", json!({}))],
        );
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments, json!({}));
    }

    /// Дословно из sglang `spark25_detector.py` (docstring wire format).
    #[test]
    fn spark25_reference_shape_parses() {
        let text = "<tool_call>get_weather\n<arg_key>city</arg_key><arg_value>Paris</arg_value>\n</tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    #[test]
    fn a_glm_call_without_closing_tag_is_reported() {
        let reply = parse_with(
            "<tool_call>get_weather\n<arg_key>city</arg_key>\n<arg_value>Paris",
            &weather(),
        );
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Строгость: `<tool_call>` в объяснении не разбирается как вызов с
    /// произвольным «именем» из следующей строки прозы.
    #[test]
    fn prose_mentioning_tool_call_is_left_alone() {
        let text = "Формат такой: <tool_call>имя вызова и его пояснение,\nа потом продолжение фразы без всякого тега.";
        let reply = parse_with(text, &weather());
        assert!(
            reply.calls.is_empty() && reply.errors.is_empty(),
            "{:?}",
            reply.errors
        );
        assert_eq!(reply.visible, text);
    }

    /// Дословно из sglang `hunyuan_detector.py` (docstring): Hy3, с
    /// `<tool_sep>`.
    #[test]
    fn hunyuan_hy3_reference_example_parses() {
        let text = "<tool_calls>\n<tool_call>get_weather<tool_sep>\n<arg_key>city</arg_key>\n<arg_value>value1</arg_value>\n</tool_call>\n</tool_calls>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].arguments["city"], "value1");
        assert!(reply.calls[0].origin.needs_person());
    }

    /// Hy4 (vLLM `hy_v4_tool_parser.py`) не шлёт `<tool_sep>`.
    #[test]
    fn hunyuan_hy4_call_without_tool_sep_parses() {
        let text = "<tool_calls><tool_call>get_weather<arg_key>city</arg_key><arg_value>Paris</arg_value></tool_call></tool_calls>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    /// Суффикс тега по чекпойнту (vLLM `detect_token_suffix`,
    /// `<tool_call:opensource>`/`:6124c78e`).
    #[test]
    fn hunyuan_tag_suffix_is_tolerated() {
        let text = "<tool_calls:opensource><tool_call:opensource>get_weather<tool_sep:opensource><arg_key:opensource>city</arg_key:opensource><arg_value:opensource>Paris</arg_value:opensource></tool_call:opensource></tool_calls:opensource>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    #[test]
    fn hunyuan_call_without_closing_tag_is_reported() {
        let text =
            "<tool_calls><tool_call>get_weather<tool_sep><arg_key>city</arg_key><arg_value>Paris";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// K2 Horizon XML (sglang/vLLM `k2_v3_detector.py`/`k2_horizon_tool_parser.py`).
    #[test]
    fn k2_horizon_xml_call_parses() {
        let text = "<ifm|tool_call>get_weather<ifm|arg_key>city</ifm|arg_key><ifm|arg_value>Paris</ifm|arg_value></ifm|tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    #[test]
    fn k2_horizon_group_with_two_calls_parses() {
        let text = "<ifm|tool_calls><ifm|tool_call>get_weather<ifm|arg_key>city</ifm|arg_key><ifm|arg_value>Paris</ifm|arg_value></ifm|tool_call><ifm|tool_call>get_weather<ifm|arg_key>city</ifm|arg_key><ifm|arg_value>Tokyo</ifm|arg_value></ifm|tool_call></ifm|tool_calls>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 2, "{:?}", reply.errors);
        assert_eq!(reply.calls[1].arguments["city"], "Tokyo");
    }

    #[test]
    fn k2_horizon_json_body_parses() {
        let text = r#"<ifm|tool_call>{"name": "get_weather", "arguments": {"city": "Paris"}}</ifm|tool_call>"#;
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    /// `<ifm|arg_type>` объявляет тип в самом вызове — важнее схемы.
    #[test]
    fn k2_horizon_arg_type_declares_the_value_type() {
        let text = "<ifm|tool_call>get_weather<ifm|arg_key>days</ifm|arg_key><ifm|arg_type>integer</ifm|arg_type><ifm|arg_value>3</ifm|arg_value></ifm|tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["days"], 3);
    }

    #[test]
    fn k2_horizon_call_without_closing_tag_is_reported() {
        let text = "<ifm|tool_call>get_weather<ifm|arg_key>city</ifm|arg_key><ifm|arg_value>Paris";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Задача: `<tool_call>\n<function=имя>\n<parameter=k>\nv\n</parameter>
    /// \n</function>\n</tool_call>` (sglang `qwen3_coder_detector.py`).
    #[test]
    fn qwen3_coder_reference_shape_parses() {
        let text = "<tool_call>\n<function=get_weather>\n<parameter=city>\nParis\n</parameter>\n</function>\n</tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
        assert!(reply.calls[0].origin.needs_person());
        assert_eq!(reply.visible, "");
    }

    /// Незакрытый `<parameter=` кончается на следующем `<parameter=`.
    #[test]
    fn qwen3_coder_unclosed_parameter_ends_at_next_parameter() {
        let text = "<tool_call><function=get_weather><parameter=city>Paris<parameter=date>2024-06-27</parameter></function></tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(
            reply.calls[0].arguments,
            json!({"city": "Paris", "date": "2024-06-27"})
        );
    }

    /// `<function=` бывает без обёртки `<tool_call>` (sglang #40236).
    #[test]
    fn qwen3_coder_bare_function_without_wrapper_parses() {
        let text = "<function=get_weather><parameter=city>Paris</parameter></function>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    #[test]
    fn qwen3_coder_call_without_closing_tag_is_reported() {
        let text = "<tool_call><function=get_weather><parameter=city>Paris";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Seed-OSS (vLLM `parser/seed_oss.py`): грамматика Qwen3 под своей
    /// обёрткой `<seed:tool_call>`.
    #[test]
    fn seed_oss_wrapper_parses() {
        let text = "<seed:tool_call><function=get_weather><parameter=city>Paris</parameter></function></seed:tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    /// Дословно из sglang `mimo_detector.py` (docstring).
    #[test]
    fn mimo_reference_shape_parses() {
        let text = "<tool_call>\n<function=execute_bash>\n<parameter=command>pwd && ls</parameter>\n</function>\n</tool_call>";
        let reply = parse_with(text, &[("execute_bash", json!({}))]);
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["command"], "pwd && ls");
    }

    /// MiMo раскодирует сущности (`html.unescape` в `_convert_param_value`).
    #[test]
    fn mimo_decodes_html_entities_in_values() {
        let text = "<tool_call><function=execute_bash><parameter=command>a &amp;&amp; b &lt;c&gt;</parameter></function></tool_call>";
        let reply = parse_with(text, &[("execute_bash", json!({}))]);
        assert_eq!(reply.calls[0].arguments["command"], "a && b <c>");
    }

    /// Step3.5 (vLLM `step3p5_tool_parser.py`) чинит `<function x>` без `=`.
    #[test]
    fn step3_5_tolerates_function_tag_without_equals() {
        let text = "<tool_call><function get_weather><parameter=city>Paris</parameter></function></tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].arguments["city"], "Paris");
    }

    /// Дословно из sglang `minicpm5_detector.py` (docstring): многострочная
    /// CDATA.
    #[test]
    fn minicpm5_reference_example_with_cdata_parses() {
        let text = "<function name=\"get_weather\"><param name=\"city\">北京</param><param name=\"date\"><![CDATA[多行\n文本]]></param></function>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].arguments["city"], "北京");
        assert_eq!(reply.calls[0].arguments["date"], "多行\n文本");
    }

    #[test]
    fn minicpm5_decodes_xml_entities_and_strips_values() {
        let text = "<function name=\"get_weather\"><param name=\"city\">  Paris &amp; more  </param></function>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls[0].arguments["city"], "Paris & more");
    }

    #[test]
    fn minicpm5_call_without_closing_tag_is_reported() {
        let text = "<function name=\"get_weather\"><param name=\"city\">Paris</param>";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty());
        assert_eq!(reply.errors.len(), 1, "{:?}", reply.errors);
    }

    /// Общий маркер `<tool_call>` делят с JSON — тело `{…}` не наше: разбирает
    /// его хермесовская грамматика (`json_wrap`), а не эта.
    #[test]
    fn a_json_body_inside_tool_call_is_left_to_the_json_family() {
        let text = "<tool_call>{\"name\": \"get_weather\", \"arguments\": {}}</tool_call>";
        let reply = parse_with(text, &weather());
        assert_eq!(reply.calls.len(), 1, "{:?}", reply.errors);
        assert_eq!(reply.calls[0].name, "get_weather");
        assert_eq!(reply.calls[0].origin.format, Some("hermes"));
    }

    /// Чужая разметка с незнакомым именем — чей-то пример, остаётся текстом.
    #[test]
    fn an_unknown_tool_name_stays_prose() {
        let text = "<tool_call>nope<arg_key>a</arg_key><arg_value>1</arg_value></tool_call>";
        let reply = parse_with(text, &weather());
        assert!(reply.calls.is_empty() && reply.errors.is_empty());
        assert_eq!(reply.visible, text);
    }
}
