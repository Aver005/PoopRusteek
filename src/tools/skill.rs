use super::*;
use crate::skills::SkillDefinition;
use serde_json::{Value, json};

pub struct SkillTool {
    pub skills: std::sync::Arc<std::sync::RwLock<Vec<SkillDefinition>>>,
}

#[async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "skill".to_string(),
            description:
                "Load or list skills. Skills are reusable capabilities that provide the agent with procedural knowledge for specific tasks. Use `list` to see available skills, or `load` to load a specific skill by name and inject its content into context.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "load"],
                        "description": "\"list\" — show all available skills with descriptions; \"load\" — load a skill's content into the conversation context"
                    },
                    "name": {
                        "type": "string",
                        "description": "The name or slug of the skill to load (only used with action=load)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let action = match args["action"].as_str() {
            Some(a) => a,
            None => return ToolResult::error("Missing 'action' argument"),
        };

        match action {
            "list" => {
                let skills = self.skills.read().unwrap();
                if skills.is_empty() {
                    return ToolResult::success("No skills available.");
                }
                let mut lines = vec!["Available skills:".to_string()];
                for skill in skills.iter() {
                    let status = if skill.enabled { " [enabled]" } else { "" };
                    lines.push(format!(
                        "  - {}: {}{}",
                        skill.name, skill.description, status
                    ));
                }
                lines.push(String::new());
                lines.push("Use `skill` tool with action=\"load\" and name=\"<skill-name>\" to load a skill.".to_string());
                ToolResult::success(&lines.join("\n"))
            }
            "load" => {
                let name = args["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    return ToolResult::error(
                        "Missing 'name' argument for action=load. Specify the skill name or slug.",
                    );
                }

                let name_lower = name.to_lowercase();
                let skills = self.skills.read().unwrap();
                let skill = skills.iter().find(|s| {
                    s.name.to_lowercase() == name_lower || s.slug.to_lowercase() == name_lower
                });

                match skill {
                    Some(s) => {
                        ToolResult::success(&format!("# Skill: {}\n{}", s.name, s.content.trim()))
                    }
                    None => {
                        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
                        ToolResult::error(&format!(
                            "Skill '{name}' not found. Available skills: {}",
                            names.join(", ")
                        ))
                    }
                }
            }
            _ => ToolResult::error(&format!(
                "Unknown action: '{action}'. Use 'list' or 'load'."
            )),
        }
    }
}
