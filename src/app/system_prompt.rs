//! System-prompt assembly.
//!
//! Builds the agent's system prompt from its actual inputs — the prompt
//! templates, enabled skills, the tool registry, the MCP manager, and the
//! workspace path — taken as explicit narrow parameters rather than reaching
//! into all of `App`. One section-builder per template slot; `build` only
//! gathers inputs and substitutes.

use crate::config::{McpSchemaMode, SkillInjectionMode};
use crate::mcp::MCPManager;
use crate::mcp::manager::FullMCPTool;
use crate::mcp::types::MCPResource;
use crate::prompts::PromptFiles;
use crate::skills::SkillDefinition;
use crate::tools::registry::ToolRegistry;

pub async fn build(
    prompts: &PromptFiles,
    skills: &[SkillDefinition],
    skills_injection: SkillInjectionMode,
    tools: &ToolRegistry,
    mcp: &tokio::sync::Mutex<MCPManager>,
    mcp_schema_mode: McpSchemaMode,
    workspace: &str,
) -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string());
    let os = std::env::consts::OS.to_string();

    // Snapshot the manager's lists and drop the lock before building any
    // text — the sections are pure, so there is no reason to hold it.
    let (all_mcp_tools, all_mcp_resources) = {
        let mcp_guard = mcp.lock().await;
        (mcp_guard.get_all_tools(), mcp_guard.get_all_resources())
    };
    let mcp_deferred = mcp_schema_mode.deferred_for(all_mcp_tools.len());

    let builtin_section = builtin_section(tools);
    let mcp_section = format!(
        "{}{}",
        mcp_tool_section(&all_mcp_tools, mcp_deferred),
        mcp_resource_section(&all_mcp_resources, mcp_deferred)
    );

    let base_prompt = prompts
        .base_prompt
        .replace("{{user}}", &user)
        .replace("{{folder}}", workspace)
        .replace("{{os}}", &os);
    let tools_prompt = prompts
        .tools_prompt
        .replace("{{builtin_tools}}", &builtin_section)
        .replace("{{mcp_tools}}", &mcp_section);

    let skills_section =
        crate::skills::discovery::load_enabled_skills_content(skills, skills_injection);

    let assembled = format!(
        "{}\n\n{}{}",
        base_prompt.trim(),
        tools_prompt.trim(),
        skills_section
    );
    // Prompt-size telemetry: every DeepSeek session pays this up front as
    // flat text, so section growth is worth noticing before users do.
    crate::debug_log::log(
        "system_prompt.assembled",
        format!(
            "bytes: base={} tools={} builtin_defs={} mcp={} skills={} total={}",
            base_prompt.len(),
            tools_prompt.len(),
            builtin_section.len(),
            mcp_section.len(),
            skills_section.len(),
            assembled.len()
        ),
    );
    assembled
}

/// `{{builtin_tools}}`: full definitions of every builtin tool.
fn builtin_section(tools: &ToolRegistry) -> String {
    let builtin_tools = tools.definitions();
    if builtin_tools.is_empty() {
        return "- none".to_string();
    }
    builtin_tools
        .iter()
        .map(|tool| {
            crate::util::format_tool_definition(&tool.name, &tool.description, &tool.parameters)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Tool half of `{{mcp_tools}}`: full definitions — or, deferred, a
/// server-level summary ONLY. Individual tools are not listed at all:
/// semantic hints attach full definitions of the tools relevant to each
/// request, and `tool_search` covers explicit lookup, so up-front
/// enumeration is pure context waste (a couple of servers is already 50+
/// lines).
fn mcp_tool_section(all_mcp_tools: &[FullMCPTool], deferred: bool) -> String {
    if all_mcp_tools.is_empty() {
        return "- none".to_string();
    }
    if deferred {
        let mut per_server: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for full in all_mcp_tools {
            let server = full
                .full_name
                .strip_prefix(crate::mcp::MCP_TOOL_PREFIX)
                .and_then(|rest| rest.split("__").next())
                .unwrap_or("?");
            *per_server.entry(server).or_default() += 1;
        }
        let mut lines = vec![format!(
            "{} MCP tools from {} connected server(s) are available but NOT listed here. Relevant tools (with full parameter schemas) are suggested to you automatically per request. To find one yourself, call `tool_search` with a short description of the capability you need (any language) — it returns exact tool names (`mcp__<server>__<tool>`) and their schemas. Never guess or invent MCP tool names.\n\nConnected servers:",
            all_mcp_tools.len(),
            per_server.len(),
        )];
        for (server, count) in &per_server {
            lines.push(format!("- {server} ({count} tools)"));
        }
        return lines.join("\n");
    }
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
}

/// Resource half of `{{mcp_tools}}`: an itemized catalog — or, deferred, a
/// count. Same rationale as deferred tool schemas: an itemized resource
/// catalog is up-front context spend with no read path in the agent loop
/// today.
fn mcp_resource_section(all_mcp_resources: &[MCPResource], deferred: bool) -> String {
    if all_mcp_resources.is_empty() {
        return String::new();
    }
    if deferred {
        return format!(
            "\n\n{} MCP resource(s) from the connected servers exist but are not listed here.",
            all_mcp_resources.len()
        );
    }
    let mut lines = vec!["\n\n### Available MCP resources:".to_string()];
    for r in all_mcp_resources {
        let desc = r.description.as_deref().unwrap_or("");
        let name = if r.name.is_empty() { &r.uri } else { &r.name };
        lines.push(format!("- `{}`: {} ({})", name, desc, r.uri));
    }
    lines.join("\n")
}
