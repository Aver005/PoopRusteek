use regex::Regex;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: Value,
}

pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    let xml_pattern = Regex::new(r"(?s)<tool_use>\s*(.*?)\s*</tool_use>").unwrap();
    let name_pattern = Regex::new(r"(?s)<name>\s*(.*?)\s*</name>").unwrap();
    let args_pattern = Regex::new(r"(?s)<arguments>\s*(.*?)\s*</arguments>").unwrap();
    let legacy_pattern = Regex::new(r"\[TOOL:([^\]]+)\]\s*(\{[^}]*\})").unwrap();

    for cap in xml_pattern.captures_iter(text) {
        let body = cap[1].trim();

        if let (Some(name_cap), Some(args_cap)) = (
            name_pattern.captures(body),
            args_pattern.captures(body),
        ) {
            let name = name_cap[1].trim().to_string();
            let args_str = args_cap[1].trim();
            match serde_json::from_str::<Value>(args_str) {
                Ok(arguments) => calls.push(ParsedToolCall { name, arguments }),
                Err(error) => tracing::warn!("Failed to parse <tool_use> arguments: {error}"),
            }
            continue;
        }

        match serde_json::from_str::<Value>(body) {
            Ok(value) => {
                let name = value
                    .get("tool")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let arguments = value.get("args").cloned().unwrap_or(Value::Object(Default::default()));
                if let Some(name) = name {
                    calls.push(ParsedToolCall { name, arguments });
                }
            }
            Err(error) => tracing::warn!("Failed to parse <tool_use> body: {error}"),
        }
    }

    for cap in legacy_pattern.captures_iter(text) {
        let name = cap[1].to_string();
        let args_str = &cap[2];

        match serde_json::from_str::<Value>(args_str) {
            Ok(args) => {
                calls.push(ParsedToolCall { name, arguments: args });
            }
            Err(e) => {
                tracing::warn!("Failed to parse tool arguments for '{name}': {e}");
            }
        }
    }

    calls
}

pub fn has_tool_calls(text: &str) -> bool {
    text.contains("<tool_use>") || text.contains("[TOOL:")
}

pub fn strip_tool_calls(text: &str) -> String {
    let xml_pattern = Regex::new(r"(?s)<tool_use>\s*.*?\s*</tool_use>").unwrap();
    let legacy_pattern = Regex::new(r"\[TOOL:[^\]]+\]\s*\{[^}]*\}").unwrap();
    let without_xml = xml_pattern.replace_all(text, "");
    legacy_pattern.replace_all(without_xml.trim(), "").trim().to_string()
}

pub fn stream_visible_text(text: &str) -> String {
    let xml_pattern = Regex::new(r"(?s)<tool_use>\s*.*?\s*</tool_use>").unwrap();
    let legacy_pattern = Regex::new(r"\[TOOL:[^\]]+\]\s*\{[^}]*\}").unwrap();
    let without_xml = xml_pattern.replace_all(text, "");
    let without_complete = legacy_pattern.replace_all(&without_xml, "");
    let mut visible = without_complete.to_string();

    let cut_markers = [
        "<tool_use",
        "</tool_use",
        "<name",
        "</name",
        "<arguments",
        "</arguments",
        "[TOOL:",
    ];

    if let Some(index) = cut_markers
        .iter()
        .filter_map(|marker| visible.find(marker))
        .min()
    {
        visible.truncate(index);
    }

    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_tool_call() {
        let text = r#"Here is the result: [TOOL:bash] {"command": "ls -la"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn test_parse_mcp_tool_call() {
        let text = r#"[TOOL:mcp__github__create_issue] {"title": "Bug report", "body": "Found a bug"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "mcp__github__create_issue");
        assert_eq!(calls[0].arguments["title"], "Bug report");
    }

    #[test]
    fn test_no_tool_calls() {
        let text = "This is just a regular response without any tools.";
        assert!(!has_tool_calls(text));
        assert!(parse_tool_calls(text).is_empty());
    }

    #[test]
    fn test_multiple_tool_calls() {
        let text = r#"[TOOL:bash] {"command": "pwd"}
Then [TOOL:file.read] {"path": "test.txt"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn test_parse_xml_tool_call() {
        let text = r#"
<tool_use>
<name>powershell</name>
<arguments>
{"command":"Get-Location"}
</arguments>
</tool_use>
"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(calls[0].arguments["command"], "Get-Location");
    }

    #[test]
    fn test_strip_tool_calls() {
        let text = "Before\n<tool_use><name>bash</name><arguments>{\"command\":\"pwd\"}</arguments></tool_use>\nAfter";
        assert_eq!(strip_tool_calls(text), "Before\n\nAfter");
    }

    #[test]
    fn test_stream_visible_text_hides_incomplete_tool_use() {
        let text = "Answer first\n<tool_use><name>bash</name>";
        assert_eq!(stream_visible_text(text), "Answer first\n");
    }

    #[test]
    fn test_stream_visible_text_hides_partial_tool_tag() {
        let text = "Answer first\n<tool_use";
        assert_eq!(stream_visible_text(text), "Answer first\n");
    }
}
