//! System-prompt assembly.
//!
//! Builds the agent's system prompt from its actual inputs — the prompt
//! templates, enabled skills, the tool registry, the MCP manager, and the
//! workspace path — taken as explicit narrow parameters rather than reaching
//! into all of `App`.

use crate::mcp::MCPManager;
use crate::prompts::PromptFiles;
use crate::skills::SkillDefinition;
use crate::tools::registry::ToolRegistry;

pub async fn build(
    prompts: &PromptFiles,
    skills: &[SkillDefinition],
    tools: &ToolRegistry,
    mcp: &tokio::sync::Mutex<MCPManager>,
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
            .map(|tool| super::format_tool_definition(&tool.name, &tool.description, &tool.parameters))
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let mcp_guard = mcp.lock().await;
    let all_mcp_tools = mcp_guard.get_all_tools();
    let all_mcp_resources = mcp_guard.get_all_resources();
    let mcp_tool_section = if all_mcp_tools.is_empty() {
        "- none".to_string()
    } else {
        all_mcp_tools
            .iter()
            .map(|full| {
                super::format_tool_definition(
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
