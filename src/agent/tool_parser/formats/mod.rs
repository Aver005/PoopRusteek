//! Грамматики форматов вызова. Каждая сообщает сканеру свой открывающий
//! маркер и разбирает находку с его позиции.

mod channels;
mod gemma;
mod invoke;
mod json_wrap;
mod kimi;
mod minimax_m3;
mod tagged;
mod tokens;
mod tool_use;
mod weak;

pub(super) use weak::whole_reply;

use serde_json::Value;
use std::ops::Range;

/// Описатель формата: из них сканер собирает общую регулярку маркеров.
pub(super) struct Family {
    /// Открывающий маркер — регулярка без якорей.
    pub opener: String,
    /// Разбор с позиции маркера. `None` — за маркером нет тела вызова, это проза.
    pub parse: fn(&str, usize) -> Option<Found>,
}

/// Порядок — порядок попыток на одной позиции: доверенные форматы первыми.
pub(super) fn families() -> Vec<Family> {
    [
        tool_use::families(),
        invoke::families(),
        tokens::families(),
        json_wrap::families(),
        tagged::families(),
        kimi::families(),
        gemma::families(),
        minimax_m3::families(),
        channels::families(),
        weak::families(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Кусок текста, который грамматика признала своей: вызовы или ошибки.
pub(super) struct Found {
    pub span: Range<usize>,
    pub items: Vec<Result<RawCall, String>>,
}

impl Found {
    fn call(span: Range<usize>, call: RawCall) -> Self {
        Self {
            span,
            items: vec![Ok(call)],
        }
    }

    fn broken(span: Range<usize>, error: String) -> Self {
        Self {
            span,
            items: vec![Err(error)],
        }
    }
}

pub(super) struct RawCall {
    pub name: String,
    pub args: RawArgs,
    /// Конкретный формат: дубль отбрасывается, только если форматы разные.
    pub format: &'static str,
}

/// Аргументы как их записала модель; типы приводятся после слияния.
pub(super) enum RawArgs {
    Json(Value),
    Params(Vec<Param>),
}

pub(super) struct Param {
    pub name: String,
    pub text: String,
    pub declared: Declared,
}

/// Тип значения, объявленный в самом вызове.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Declared {
    /// Строка как есть.
    Text,
    /// JSON, а негодный — строка (DSML `string="false"` или без атрибута).
    Json,
    /// Тип не объявлен: решает схема инструмента.
    BySchema,
}

pub(super) fn skip_ws(hay: &str, pos: usize) -> usize {
    hay[pos..]
        .find(|c: char| !c.is_whitespace())
        .map_or(hay.len(), |offset| pos + offset)
}

/// Одно JSON-значение с позиции `pos` и конец его в `hay`. Конец ищется
/// разбором: закрывающий тег внутри строки значения его не обрывает.
pub(super) fn json_at(hay: &str, pos: usize) -> Result<(Value, usize), serde_json::Error> {
    let mut values = serde_json::Deserializer::from_str(&hay[pos..]).into_iter::<Value>();
    match values.next() {
        Some(Ok(value)) => Ok((value, pos + values.byte_offset())),
        Some(Err(error)) => Err(error),
        None => serde_json::from_str::<Value>("").map(|value| (value, pos)),
    }
}
