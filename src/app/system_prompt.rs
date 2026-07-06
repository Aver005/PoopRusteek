//! System-prompt assembly.
//!
//! Builds the agent's system prompt from its actual inputs — the prompt
//! templates, enabled skills, the tool registry, the MCP manager, and the
//! workspace path — taken as explicit narrow parameters rather than reaching
//! into all of `App`.

use crate::config::McpSchemaMode;
use crate::mcp::MCPManager;
use crate::prompts::PromptFiles;
use crate::skills::SkillDefinition;
use crate::tools::registry::ToolRegistry;

/// Byte cap for one description line in the deferred MCP tool list — long
/// MCP descriptions (Playwright's run for paragraphs) would defeat the
/// point of deferring.
const DEFERRED_DESC_MAX_BYTES: usize = 160;

pub async fn build(
    prompts: &PromptFiles,
    skills: &[SkillDefinition],
    tools: &ToolRegistry,
    mcp: &tokio::sync::Mutex<MCPManager>,
    mcp_schema_mode: McpSchemaMode,
    workspace: &str,
) -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string());
    let os = std::env::consts::OS.to_string();

    let builtin_tools = tools.definitions();
    let builtin_section = if builtin_tools.is_empty() {
        "- none".to_string()
    } else {
        builtin_tools
            .iter()
            .map(|tool| {
                crate::util::format_tool_definition(&tool.name, &tool.description, &tool.parameters)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let mcp_guard = mcp.lock().await;
    let all_mcp_tools = mcp_guard.get_all_tools();
    let all_mcp_resources = mcp_guard.get_all_resources();
    let mcp_tool_section = if all_mcp_tools.is_empty() {
        "- none".to_string()
    } else if mcp_schema_mode.deferred_for(all_mcp_tools.len()) {
        // Deferred mode: names + first description line only. Full
        // definitions arrive per-turn via semantic hints or on demand via
        // the `tool_search` builtin — this saves thousands of prompt
        // tokens once a couple of servers are connected.
        let mut lines = vec![
            "The MCP tools below are listed WITHOUT parameter schemas. Before calling one for the first time, call `tool_search` with a description of what you need — it returns the full definitions of the best-matching tools. Use the exact names as listed.".to_string(),
        ];
        for full in &all_mcp_tools {
            let first_line = full.tool.description.lines().next().unwrap_or("");
            lines.push(format!(
                "- `{}`: {}",
                full.full_name,
                crate::util::truncate_with_ellipsis(first_line, DEFERRED_DESC_MAX_BYTES)
            ));
        }
        lines.join("\n")
    } else {
        all_mcp_tools
            .iter()
            .map(|full| {
                crate::util::format_tool_definition(
                    &full.full_name,
                    &full.tool.description,
                    &full.tool.input_schema,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mcp_resource_section = if all_mcp_resources.is_empty() {
        String::new()
    } else {
        let mut lines = vec!["\n\n### Available MCP resources:".to_string()];
        for r in &all_mcp_resources {
            let desc = r.description.as_deref().unwrap_or("");
            let name = if r.name.is_empty() { &r.uri } else { &r.name };
            lines.push(format!("- `{}`: {} ({})", name, desc, r.uri));
        }
        lines.join("\n")
    };
    let mcp_section = format!("{mcp_tool_section}{mcp_resource_section}");
    drop(mcp_guard);

    let base_prompt = prompts
        .base_prompt
        .replace("{{user}}", &user)
        .replace("{{folder}}", workspace)
        .replace("{{os}}", &os);
    let tools_prompt = prompts
        .tools_prompt
        .replace("{{builtin_tools}}", &builtin_section)
        .replace("{{mcp_tools}}", &mcp_section);

    let skills_section = crate::skills::discovery::load_enabled_skills_content(skills);

    format!(
        "{}\n\n{}{}",
        base_prompt.trim(),
        tools_prompt.trim(),
        skills_section
    )
}
