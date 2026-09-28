//! Инструменты, известные на этот ход: имена для сверки и схемы для типов.

use crate::mcp::{MCP_TOOL_PREFIX, MCPManager};
use crate::tools::registry::ToolRegistry;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct ToolCatalog {
    schemas: HashMap<String, Value>,
    /// Имена целыми словами — для страховки. `None` — каталог пуст.
    mentions: Option<Regex>,
}

pub(super) enum Resolved<'c> {
    Exact(&'c str),
    /// Имя совпало без учёта регистра и `-`/`_` — единственное такое.
    Renamed(&'c str),
    Unknown,
}

impl ToolCatalog {
    pub fn new(tools: impl IntoIterator<Item = (String, Value)>) -> Self {
        let schemas: HashMap<String, Value> = tools.into_iter().collect();
        let mut names: Vec<&str> = schemas.keys().map(String::as_str).collect();
        // Длинные раньше: `shell_output` не должен совпасть как `shell`.
        names.sort_by_key(|name| std::cmp::Reverse(name.len()));
        let mentions = (!names.is_empty()).then(|| {
            let names: Vec<String> = names.iter().map(|name| regex::escape(name)).collect();
            Regex::new(&format!(r"\b(?:{})\b", names.join("|")))
                .expect("escaped tool names form a valid regex")
        });
        Self { schemas, mentions }
    }

    /// Снимок на ход. Лок MCP держится только на копирование списка.
    pub async fn snapshot(tools: &ToolRegistry, mcp: &tokio::sync::Mutex<MCPManager>) -> Self {
        let mcp_tools = mcp.lock().await.get_all_tools();
        let builtin = tools
            .definitions()
            .into_iter()
            .map(|tool| (tool.name, tool.parameters));
        let mcp = mcp_tools
            .into_iter()
            .map(|tool| (tool.full_name, tool.tool.input_schema));
        Self::new(builtin.chain(mcp))
    }

    pub(super) fn resolve(&self, written: &str) -> Resolved<'_> {
        if let Some((name, _)) = self.schemas.get_key_value(written) {
            return Resolved::Exact(name);
        }
        // MCP-имя сверяется только целиком: свёрнутое, оно могло бы попасть в
        // соседний инструмент того же сервера.
        if written.starts_with(MCP_TOOL_PREFIX) {
            return Resolved::Unknown;
        }
        let fold = |name: &str| name.to_lowercase().replace('-', "_");
        let key = fold(written);
        let mut matches = self
            .schemas
            .keys()
            .filter(|name| !name.starts_with(MCP_TOOL_PREFIX) && fold(name) == key);
        match (matches.next(), matches.next()) {
            (Some(name), None) => Resolved::Renamed(name),
            _ => Resolved::Unknown,
        }
    }

    pub(super) fn schema(&self, name: &str) -> Option<&Value> {
        self.schemas.get(name)
    }

    pub(super) fn mentions(&self) -> Option<&Regex> {
        self.mentions.as_ref()
    }
}

/// Схема одного параметра инструмента.
pub(super) fn property<'s>(schema: Option<&'s Value>, name: &str) -> Option<&'s Value> {
    schema?.get("properties")?.get(name)
}

/// Значение без объявленного типа. В нестроковый тип — только если схема явно
/// его допускает: иначе «007» стало бы 7, а «1.10» — 1.1.
pub(super) fn coerce(text: &str, property: Option<&Value>) -> Value {
    let types = property.map(allowed_types).unwrap_or_default();
    let non_string = ["integer", "number", "boolean", "object", "array"];
    if types.iter().any(|kind| non_string.contains(kind))
        && let Ok(value) = serde_json::from_str::<Value>(text.trim())
        && fits(&value, &types)
    {
        return value;
    }
    Value::String(text.to_string())
}

fn allowed_types(property: &Value) -> Vec<&str> {
    let mut types = Vec::new();
    push_types(property, &mut types);
    for key in ["anyOf", "oneOf"] {
        for variant in property
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            push_types(variant, &mut types);
        }
    }
    types
}

fn push_types<'v>(schema: &'v Value, types: &mut Vec<&'v str>) {
    match schema.get("type") {
        Some(Value::String(kind)) => types.push(kind),
        Some(Value::Array(kinds)) => types.extend(kinds.iter().filter_map(Value::as_str)),
        _ => {}
    }
}

fn fits(value: &Value, types: &[&str]) -> bool {
    types.iter().any(|kind| match *kind {
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> ToolCatalog {
        ToolCatalog::new([
            ("read_file".to_string(), json!({})),
            ("shell_output".to_string(), json!({})),
            ("mcp__gh__get_issue".to_string(), json!({})),
            ("mcp__gh__get-issue".to_string(), json!({})),
        ])
    }

    #[test]
    fn names_resolve_exactly_then_folded_but_never_for_mcp() {
        let catalog = catalog();
        assert!(matches!(
            catalog.resolve("read_file"),
            Resolved::Exact("read_file")
        ));
        assert!(matches!(
            catalog.resolve("Read-File"),
            Resolved::Renamed("read_file")
        ));
        assert!(matches!(
            catalog.resolve("mcp__gh__Get_Issue"),
            Resolved::Unknown
        ));
        assert!(matches!(catalog.resolve("nope"), Resolved::Unknown));
    }

    #[test]
    fn values_turn_non_string_only_when_the_schema_says_so() {
        let int = json!({"type": "integer"});
        let text = json!({"type": ["string", "null"]});
        let either = json!({"anyOf": [{"type": "integer"}, {"type": "string"}]});
        assert_eq!(coerce("3", Some(&int)), json!(3));
        assert_eq!(coerce("007", Some(&int)), json!("007"));
        assert_eq!(coerce("null", Some(&text)), json!("null"));
        assert_eq!(coerce("1.10", None), json!("1.10"));
        assert_eq!(coerce(" 42 ", Some(&either)), json!(42));
        assert_eq!(coerce("x", Some(&int)), json!("x"));
    }
}
