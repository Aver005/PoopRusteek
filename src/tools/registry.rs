use super::*;
use crate::skills::SkillDefinition;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct ToolRegistry {
    tools: Mutex<HashMap<String, Arc<dyn Tool>>>,
    pub skill_tool: Mutex<Option<Arc<skill::SkillTool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let registry = Self {
            tools: Mutex::new(HashMap::new()),
            skill_tool: Mutex::new(None),
        };
        registry.register_default_tools();
        registry
    }

    fn register_default_tools(&self) {
        // PowerShell is Windows-only; on non-Windows only bash is on PATH.
        // Windows dev boxes commonly also have Git Bash, so register both there.
        if cfg!(windows) {
            self.register(Arc::new(shell::ShellTool::powershell()));
            self.register(Arc::new(shell::ShellTool::bash()));
        } else {
            self.register(Arc::new(shell::ShellTool::bash()));
        }
        self.register(Arc::new(question::QuestionTool));
        self.register(Arc::new(task::TaskTool));
        self.register(Arc::new(shell_control::ShellOutputTool));
        self.register(Arc::new(shell_control::ShellKillTool));
        self.register(Arc::new(shell_control::ShellListTool));
        self.register(Arc::new(shell_control::ShellInputTool));
    }

    pub fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        self.tools.lock().unwrap().insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.lock().unwrap().get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.lock().unwrap().values().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, name: &str, args: Value) -> ToolResult {
        match self.get(name) {
            Some(tool) => tool.execute(args).await,
            None => ToolResult::error(&format!("Unknown tool: {name}")),
        }
    }

    pub fn update_skills(&self, skills: Vec<SkillDefinition>) {
        let shared = Arc::new(std::sync::RwLock::new(skills));
        let skill_tool = Arc::new(skill::SkillTool {
            skills: Arc::clone(&shared),
        });
        *self.skill_tool.lock().unwrap() = Some(Arc::clone(&skill_tool));
        self.tools.lock().unwrap().insert("skill".to_string(), skill_tool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_is_always_registered() {
        // bash is on PATH on every supported platform (Git Bash on Windows
        // dev boxes, native bash elsewhere) — registered unconditionally.
        let registry = ToolRegistry::new();
        assert!(registry.get("bash").is_some());
    }

    #[test]
    fn powershell_registration_matches_the_host_platform() {
        // PowerShell only exists on Windows; on non-Windows hosts it must
        // not be registered (there's no interpreter on PATH to run it).
        let registry = ToolRegistry::new();
        assert_eq!(registry.get("powershell").is_some(), cfg!(windows));
    }
}
