//! Registry-inspection UI glue: the `/whitelist` and `/skills` pickers and
//! the markdown summaries behind `/tools` and `/ps` (built-in + MCP tools,
//! background jobs). Pure presentation over the tool/skill/job registries —
//! no agent or session state is mutated here beyond opening a picker modal.

use super::App;
use crate::util::format_duration_secs;
use std::collections::HashSet;

impl App {
    pub(super) async fn open_whitelist_picker(&mut self) {
        use crate::app::events::{PickerItem, PickerKind, PickerMode, PickerState};
        let mut items: Vec<PickerItem> = Vec::new();
        let mut checked: Vec<usize> = Vec::new();
        let whitelist: HashSet<String> = crate::whitelist::load();

        // Built-in tools
        for def in self.tools.definitions() {
            let in_list = whitelist.contains(&def.name);
            items.push(PickerItem::new(
                format!(
                    "{}  {}",
                    if in_list { "\u{2611}" } else { "\u{2610}" },
                    def.name
                ),
                def.name.clone(),
            ));
            if in_list {
                checked.push(items.len() - 1);
            }
        }

        // MCP tools
        let mcp = self.mcp.lock().await;
        for full in mcp.get_all_tools() {
            let in_list = whitelist.contains(&full.full_name);
            items.push(PickerItem::new(
                format!(
                    "{}  {}",
                    if in_list { "\u{2611}" } else { "\u{2610}" },
                    full.full_name
                ),
                full.full_name.clone(),
            ));
            if in_list {
                checked.push(items.len() - 1);
            }
        }
        drop(mcp);

        if items.is_empty() {
            self.state
                .focused_mut()
                .messages
                .push(crate::provider::ChatMessage::system(
                    "No tools available to whitelist.",
                ));
            return;
        }

        let mut picker = PickerState::new_with_kind(
            " Tool Whitelist (Space to toggle, Enter to save)",
            items,
            PickerMode::Multi,
            PickerKind::Whitelist,
        );
        picker.checked = checked;
        picker.persistent_checked = whitelist.into_iter().collect();
        self.state.modal = Some(crate::app::events::Modal::Picker(picker));
    }

    pub(super) async fn open_skill_picker(&mut self) {
        use crate::app::events::{PickerItem, PickerKind, PickerMode, PickerState};
        let mut items: Vec<PickerItem> = Vec::new();
        let mut checked: Vec<usize> = Vec::new();
        let enabled_slugs: HashSet<String> = self.config.skills.enabled.iter().cloned().collect();

        for skill in &self.skills {
            let is_enabled =
                enabled_slugs.contains(&skill.slug) || enabled_slugs.contains(&skill.name);
            let status = if is_enabled { "\u{2611}" } else { "\u{2610}" };
            items.push(PickerItem::new(
                format!(
                    "{}  {}  [{}] {}",
                    status, skill.name, skill.source, skill.description
                ),
                skill.slug.clone(),
            ));
            if is_enabled {
                checked.push(items.len() - 1);
            }
        }

        if items.is_empty() {
            self.state.focused_mut().messages.push(crate::provider::ChatMessage::system(
                "No skills found. Install skills with `npx skills add <owner/repo>` or create SKILL.md files in `.skills/` directory.",
            ));
            return;
        }

        let mut picker = PickerState::new_with_kind(
            " Skills (Space to toggle, Enter to save)",
            items,
            PickerMode::Multi,
            PickerKind::Skills,
        );
        picker.checked = checked;
        picker.persistent_checked = self.config.skills.enabled.clone();
        self.state.modal = Some(crate::app::events::Modal::Picker(picker));
    }

    pub(super) async fn toggle_skill(&mut self, name: &str, enable: bool) {
        let mut changed = false;
        let name_lower = name.to_lowercase();
        for skill in &mut self.skills {
            if skill.slug.to_lowercase() == name_lower || skill.name.to_lowercase() == name_lower {
                if skill.enabled != enable {
                    skill.enabled = enable;
                    changed = true;
                }
                break;
            }
        }

        if changed {
            let enabled: Vec<String> = self
                .skills
                .iter()
                .filter(|s| s.enabled)
                .map(|s| s.slug.clone())
                .collect();
            self.config.skills.enabled = enabled;
            // Save the in-memory config — reloading a fresh copy from disk
            // here used to silently clobber any other unsaved config change.
            if let Err(e) = crate::config::save(&self.config) {
                tracing::warn!("Failed to save skills config: {e}");
            }
            self.tools.update_skills(self.skills.clone());

            let msg = if enable {
                format!(
                    "Skill '{name}' enabled. Its content will be included in the system prompt on next request."
                )
            } else {
                format!("Skill '{name}' disabled.")
            };
            self.state
                .focused_mut()
                .messages
                .push(crate::provider::ChatMessage::system(&msg));
        } else if enable {
            self.state
                .focused_mut()
                .messages
                .push(crate::provider::ChatMessage::system(&format!(
                    "Skill '{name}' not found. Use /skills list to see available skills."
                )));
        }
    }

    pub(super) async fn build_tools_display(&self) -> String {
        let mut lines = vec!["## Available Tools".to_string()];

        let builtin = self.tools.definitions();
        if builtin.is_empty() {
            lines.push("\n### Built-in tools".to_string());
            lines.push("- none".to_string());
        } else {
            lines.push(format!("\n### Built-in tools ({})", builtin.len()));
            for tool in &builtin {
                lines.push(crate::util::format_tool_definition(
                    &tool.name,
                    &tool.description,
                    &tool.parameters,
                ));
            }
        }

        let mcp = self.mcp.lock().await;
        let all_mcp = mcp.get_all_tools();
        if all_mcp.is_empty() {
            lines.push("\n### MCP tools".to_string());
            lines.push("- none".to_string());
        } else {
            let servers = mcp.get_servers_info();
            let enabled_count = servers.iter().filter(|s| s.enabled).count();
            lines.push(format!(
                "\n### MCP tools ({all} total, {enabled} enabled, {conn} connected)",
                all = all_mcp.len(),
                enabled = enabled_count,
                conn = servers
                    .iter()
                    .filter(|s| s.enabled && s.status == "connected")
                    .count(),
            ));
            for full in &all_mcp {
                let server_name = &full.tool.server_name;
                let server_info = servers.iter().find(|s| s.name == *server_name);
                let status = server_info.map(|s| s.status.as_str()).unwrap_or("unknown");
                lines.push(format!("  *Server: `{}` ({status})*", server_name));
                lines.push(crate::util::format_tool_definition(
                    &full.full_name,
                    &full.tool.description,
                    &full.tool.input_schema,
                ));
            }
        }
        drop(mcp);

        lines.join("\n\n")
    }

    pub(super) async fn build_background_processes_display(&self) -> String {
        let snapshots = crate::tools::background::process_snapshots().await;
        if snapshots.is_empty() {
            return "## Jobs\n\n- none".to_string();
        }

        let now = chrono::Utc::now();
        let running = snapshots
            .iter()
            .filter(|proc| {
                matches!(
                    proc.status,
                    crate::tools::background::ProcessStatus::Running
                )
            })
            .count();
        let interactive = snapshots.iter().filter(|proc| proc.interactive).count();
        let persistent = snapshots.iter().filter(|proc| proc.persistent).count();

        let mut lines = vec![format!(
            "## Jobs\n\n- total: {}\n- running: {}\n- interactive: {}\n- persistent: {}",
            snapshots.len(),
            running,
            interactive,
            persistent
        )];

        for proc in snapshots {
            let pid = proc
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string());
            let kind = if proc.interactive {
                "interactive"
            } else {
                "background"
            };
            let persist = if proc.persistent { " persistent" } else { "" };
            let age = format_duration_secs(
                now.signed_duration_since(proc.started_at)
                    .num_seconds()
                    .max(0) as u64,
            );
            let idle = format_duration_secs(
                now.signed_duration_since(proc.last_activity_at)
                    .num_seconds()
                    .max(0) as u64,
            );
            let ttl = match proc.ttl_secs {
                Some(0) => " ttl=off".to_string(),
                Some(ttl) => format!(" ttl={}", format_duration_secs(ttl)),
                None => String::new(),
            };
            let preview: String = proc.command.chars().take(120).collect();
            lines.push(format!(
                "- id={} pid={} [{}] {}{} {} age={} idle={}{}: `{}`",
                proc.id,
                pid,
                proc.shell,
                kind,
                persist,
                proc.status.label(),
                age,
                idle,
                ttl,
                preview
            ));
        }

        lines.join("\n")
    }
}
