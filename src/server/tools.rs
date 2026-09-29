//! OpenAI tool calling поверх любого бэкенда. Запись с родным протоколом
//! получает инструменты полем запроса (`declare`); остальным они описываются в
//! промпте, а ответ разбирает `agent::tool_parser`. Исполняет вызовы клиент.

use crate::agent::tool_parser::{ParseCtx, ToolCatalog, parse_text};
use crate::provider::openai_compat::{WireToolCall, split_reasoning};
use crate::provider::{ChatMessage, CompletionRequest, Role, ToolCall};
use crate::tools::ToolDefinition;
use serde_json::Value;

const FORMAT: &str = "# Tools

You can call the tools listed below. To call one, write exactly this block:

<tool_use>
<name>TOOL_NAME</name>
<arguments>
{\"parameter\": \"value\"}
</arguments>
</tool_use>

- Use a tool name exactly as listed; never invent one.
- `arguments` is one JSON object that matches the tool's parameter schema.
- Several independent calls may follow each other in one reply.
- Write nothing after the last </tool_use>: the result comes in the next message. Never make up a result.
- When no tool is needed, answer in plain text without any <tool_use> block.

## Available tools";

const REMINDER: &str = "Reminder: to use a tool, reply with <tool_use><name>…</name><arguments>{JSON}</arguments></tool_use> and stop after </tool_use>.";

/// Как клиент разрешил звать инструменты. `none` моста не создаёт вовсе.
#[derive(Debug, Clone, PartialEq)]
enum ToolChoice {
    Auto,
    Required,
    Forced(String),
}

pub(super) struct ToolBridge {
    tools: Vec<ToolDefinition>,
    choice: ToolChoice,
    catalog: ToolCatalog,
}

/// Разобранный ответ модели в форме OpenAI.
#[derive(Debug)]
pub(super) struct BridgedReply {
    pub reasoning: Option<String>,
    pub content: String,
    pub calls: Vec<WireToolCall>,
}

impl ToolBridge {
    /// `None` — инструментов нет или `tool_choice: "none"`.
    pub fn from_wire(
        tools: Option<&Value>,
        choice: Option<&Value>,
    ) -> Result<Option<Self>, String> {
        let tools = match tools {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(tool_definition)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("`tools` must be an array".to_string()),
        };
        let Some(choice) = tool_choice(choice)? else {
            return Ok(None);
        };
        if tools.is_empty() {
            return Ok(None);
        }
        if let ToolChoice::Forced(name) = &choice
            && !tools.iter().any(|tool| &tool.name == name)
        {
            return Err(format!("`tool_choice` names an unknown tool `{name}`"));
        }
        let catalog = ToolCatalog::new(
            tools
                .iter()
                .map(|tool| (tool.name.clone(), tool.parameters.clone())),
        );
        Ok(Some(Self {
            tools,
            choice,
            catalog,
        }))
    }

    /// Родной протокол: инструменты — полем запроса, история остаётся структурой.
    /// `tool_choice` туда не передаётся — у `CompletionRequest` нет такого поля.
    pub fn declare(&self, request: &mut CompletionRequest) {
        request.tools = self.tools.clone();
    }

    /// Ответ родного протокола: вызовы пришли структурой, id — провайдера.
    pub fn native_reply(text: &str, calls: &[ToolCall]) -> BridgedReply {
        let (reasoning, content) = split_reasoning(text);
        BridgedReply {
            reasoning,
            content: content.trim().to_string(),
            calls: calls.iter().map(WireToolCall::from_call).collect(),
        }
    }

    /// Описать инструменты в системном промпте и напомнить формат в конце.
    pub fn prepare(&self, request: &mut CompletionRequest) {
        let section = self.section();
        match request
            .messages
            .iter_mut()
            .find(|message| message.role == Role::System)
        {
            Some(system) => system.content = format!("{}\n\n{section}", system.content.trim_end()),
            None => request.messages.insert(0, ChatMessage::system(&section)),
        }
        let note = match &self.choice {
            ToolChoice::Auto => REMINDER.to_string(),
            ToolChoice::Required => format!("{REMINDER} You MUST call at least one tool now."),
            ToolChoice::Forced(name) => format!("{REMINDER} You MUST call the tool `{name}` now."),
        };
        request.messages.push(ChatMessage::system(&note));
    }

    fn section(&self) -> String {
        let tools: Vec<String> = self
            .tools
            .iter()
            .map(|tool| {
                format!(
                    "- `{}`: {}\n  Parameters (JSON Schema): {}",
                    tool.name, tool.description, tool.parameters
                )
            })
            .collect();
        format!("{FORMAT}\n\n{}", tools.join("\n"))
    }

    /// Начало вызова, на котором оборвался ответ (см. `agent::continuation`).
    pub fn cut_at(&self, text: &str, request: &CompletionRequest) -> Option<usize> {
        parse_text(text, &self.ctx(request)).cut_at
    }

    pub fn parse(&self, text: &str, request: &CompletionRequest) -> BridgedReply {
        let (reasoning, answer) = split_reasoning(text);
        let parsed = parse_text(&answer, &self.ctx(request));
        let calls: Vec<WireToolCall> = parsed
            .calls
            .iter()
            .map(|call| {
                WireToolCall::from_call(&ToolCall {
                    id: call_id(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    provider_state: None,
                })
            })
            .collect();
        // Без вызовов отдаём текст как есть: сломанная разметка лучше пустоты.
        let content = if calls.is_empty() {
            answer.trim().to_string()
        } else {
            parsed.visible
        };
        BridgedReply {
            reasoning,
            content,
            calls,
        }
    }

    fn ctx<'a>(&'a self, request: &'a CompletionRequest) -> ParseCtx<'a> {
        ParseCtx {
            catalog: &self.catalog,
            tool_outputs: request
                .messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .map(|message| message.content.as_str())
                .collect(),
            // Исполняет клиент со своим подтверждением; решать здесь некому.
            unattended: false,
        }
    }
}

/// Вызовы из истории клиента — текстом `<tool_use>` в ответе ассистента:
/// промптовый путь родных `tool_calls` не знает, а модель должна видеть, что
/// она вызывала и в каком формате.
pub(super) fn flatten_history(messages: &mut [ChatMessage]) {
    for message in messages
        .iter_mut()
        .filter(|message| !message.tool_calls.is_empty())
    {
        let blocks: Vec<String> = message.tool_calls.iter().map(tool_use_block).collect();
        let text = message.content.trim();
        message.content = if text.is_empty() {
            blocks.join("\n")
        } else {
            format!("{text}\n{}", blocks.join("\n"))
        };
        message.tool_calls.clear();
    }
}

fn tool_use_block(call: &ToolCall) -> String {
    format!(
        "<tool_use>\n<name>{}</name>\n<arguments>\n{}\n</arguments>\n</tool_use>",
        call.name, call.arguments
    )
}

fn call_id() -> String {
    format!("call_{}", uuid::Uuid::new_v4().simple())
}

fn tool_definition(tool: &Value) -> Result<ToolDefinition, String> {
    // `{"type":"function","function":{…}}`; голый `{name,…}` тоже принимаем.
    let function = tool.get("function").unwrap_or(tool);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or("every tool needs a `function.name`")?;
    Ok(ToolDefinition {
        name: name.to_string(),
        description: function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        parameters: function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
    })
}

/// `None` — клиент запретил инструменты.
fn tool_choice(choice: Option<&Value>) -> Result<Option<ToolChoice>, String> {
    match choice {
        None | Some(Value::Null) => Ok(Some(ToolChoice::Auto)),
        Some(Value::String(mode)) => match mode.as_str() {
            "auto" => Ok(Some(ToolChoice::Auto)),
            "required" | "any" => Ok(Some(ToolChoice::Required)),
            "none" => Ok(None),
            other => Err(format!("unsupported `tool_choice` \"{other}\"")),
        },
        Some(object) => object
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .map(|name| Some(ToolChoice::Forced(name.to_string())))
            .ok_or_else(|| "`tool_choice` object needs `function.name`".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Value {
        json!([{
            "type": "function",
            "function": {
                "name": "write",
                "description": "Write a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string", "enum": ["a", "b"]}, "size": {"type": "integer"}},
                    "required": ["path"]
                }
            }
        }])
    }

    fn bridge() -> ToolBridge {
        ToolBridge::from_wire(Some(&tools()), None)
            .unwrap()
            .unwrap()
    }

    fn request(messages: Vec<ChatMessage>) -> CompletionRequest {
        CompletionRequest {
            messages,
            tools: Vec::new(),
            model: "m".to_string(),
            temperature: 0.0,
            max_tokens: 10,
            stream: false,
        }
    }

    #[test]
    fn choice_none_and_empty_tools_mean_no_bridge() {
        assert!(ToolBridge::from_wire(None, None).unwrap().is_none());
        assert!(
            ToolBridge::from_wire(Some(&json!([])), None)
                .unwrap()
                .is_none()
        );
        assert!(
            ToolBridge::from_wire(Some(&tools()), Some(&json!("none")))
                .unwrap()
                .is_none()
        );
        assert!(ToolBridge::from_wire(Some(&json!({})), None).is_err());
        let unknown = json!({"type": "function", "function": {"name": "nope"}});
        assert!(ToolBridge::from_wire(Some(&tools()), Some(&unknown)).is_err());
    }

    /// Схема уходит целиком: плоский список параметров потерял бы `enum`.
    #[test]
    fn prepare_appends_the_full_schema_and_a_trailing_reminder() {
        let mut request = request(vec![
            ChatMessage::system("client rules"),
            ChatMessage::user("go"),
        ]);
        let forced = json!({"type": "function", "function": {"name": "write"}});
        ToolBridge::from_wire(Some(&tools()), Some(&forced))
            .unwrap()
            .unwrap()
            .prepare(&mut request);
        let system = &request.messages[0].content;
        assert!(system.starts_with("client rules\n\n# Tools"));
        assert!(system.contains(r#""enum":["a","b"]"#), "{system}");
        let last = request.messages.last().unwrap();
        assert_eq!(last.role, Role::System);
        assert!(last.content.contains("MUST call the tool `write`"));
    }

    #[test]
    fn a_reply_with_a_call_becomes_tool_calls() {
        let text = "<thinking>plan</thinking>\n\nWriting.\n<tool_use><name>write</name><arguments>{\"path\": \"a\", \"size\": \"3\"}</arguments></tool_use>";
        let reply = bridge().parse(text, &request(Vec::new()));
        assert_eq!(reply.reasoning.as_deref(), Some("plan"));
        assert_eq!(reply.content, "Writing.");
        assert_eq!(reply.calls.len(), 1);
        let call = &reply.calls[0];
        assert!(call.id.starts_with("call_"));
        assert_eq!(call.function.name, "write");
        let arguments: Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(arguments, json!({"path": "a", "size": "3"}));
    }

    /// Родная разметка DeepSeek тоже доходит до клиента вызовом.
    #[test]
    fn dsml_markup_is_understood_too() {
        let text = "<｜DSML｜invoke name=\"write\"><｜DSML｜parameter name=\"path\" string=\"true\">b</｜DSML｜parameter></｜DSML｜invoke>";
        let reply = bridge().parse(text, &request(Vec::new()));
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.content, "");
    }

    #[test]
    fn plain_prose_stays_content() {
        let reply = bridge().parse("The file is fine.", &request(Vec::new()));
        assert!(reply.calls.is_empty());
        assert_eq!(reply.content, "The file is fine.");
    }

    #[test]
    fn history_calls_turn_into_tool_use_text() {
        let mut assistant = ChatMessage::assistant("Let me look.");
        assistant.tool_calls = vec![ToolCall {
            id: "call_1".into(),
            name: "write".into(),
            arguments: json!({"path": "a"}),
            provider_state: None,
        }];
        let mut messages = vec![assistant];
        flatten_history(&mut messages);
        assert!(messages[0].tool_calls.is_empty());
        assert_eq!(
            messages[0].content,
            "Let me look.\n<tool_use>\n<name>write</name>\n<arguments>\n{\"path\":\"a\"}\n</arguments>\n</tool_use>"
        );
    }

    #[test]
    fn declare_puts_the_tools_into_the_request_and_nothing_into_the_prompt() {
        let mut request = request(vec![ChatMessage::system("rules"), ChatMessage::user("go")]);
        bridge().declare(&mut request);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "write");
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].content, "rules");
    }

    #[test]
    fn a_native_reply_keeps_the_provider_ids() {
        let calls = [ToolCall {
            id: "toolu_1".into(),
            name: "write".into(),
            arguments: json!({"path": "a"}),
            provider_state: None,
        }];
        let reply = ToolBridge::native_reply("<think>hm</think>Writing.", &calls);
        assert_eq!(reply.reasoning.as_deref(), Some("hm"));
        assert_eq!(reply.content, "Writing.");
        assert_eq!(reply.calls[0].id, "toolu_1");
        assert_eq!(reply.calls[0].function.arguments, r#"{"path":"a"}"#);
    }

    #[test]
    fn a_cut_call_is_seen_by_the_bridge() {
        let bridge = bridge();
        let cut = "<tool_use><name>write</name><arguments>{\"path\": \"a";
        assert_eq!(bridge.cut_at(cut, &request(Vec::new())), Some(0));
    }
}
