pub mod defs;

use crate::app::AppState;
use crate::config::Config;
use std::collections::HashMap;

pub struct CommandRegistry {
    commands: HashMap<String, Box<dyn Command>>,
}

pub trait Command: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn usage(&self) -> &str {
        ""
    }
    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult;
}

pub enum CommandResult {
    Handled,
    NeedsAgent(String),
    LoadSession(String),
    ResetProvider,
    Error(String),
    TtlUpdate(u64),
    ReloadMcp,
    ShowTools,
    OpenWhitelist,
    ShowSkills,
    ToggleSkill(String, bool),
}

#[derive(Debug, Clone)]
pub struct CommandSuggestion {
    pub name: String,
    pub description: String,
    pub usage: String,
}

impl CommandRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            commands: HashMap::new(),
        };
        registry.register_defaults();
        registry
    }

    fn register_defaults(&mut self) {
        self.register(Box::new(defs::help::HelpCommand));
        self.register(Box::new(defs::home::HomeCommand));
        self.register(Box::new(defs::cwd::CwdCommand { name: "cwd" }));
        self.register(Box::new(defs::cwd::CwdCommand { name: "cd" }));
        self.register(Box::new(defs::cwd::CwdCommand { name: "move" }));
        self.register(Box::new(defs::clear::ClearCommand));
        self.register(Box::new(defs::quit::QuitCommand));
        self.register(Box::new(defs::version::VersionCommand));
        self.register(Box::new(defs::compact::CompactCommand));
        self.register(Box::new(defs::last::LastCommand));
        self.register(Box::new(defs::load::LoadCommand));
        self.register(Box::new(defs::session_info::SessionInfoCommand));
        self.register(Box::new(defs::session_list::SessionListCommand));
        self.register(Box::new(defs::reset::ResetCommand));
        self.register(Box::new(defs::rate::RateCommand));
        self.register(Box::new(defs::retry::RetryCommand));
        self.register(Box::new(defs::mcp::McpCommand));
        self.register(Box::new(defs::tools::ToolsCommand));
        self.register(Box::new(defs::whitelist::WhitelistCommand));
        self.register(Box::new(defs::skills::SkillsCommand));
    }

    pub fn register(&mut self, cmd: Box<dyn Command>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    pub fn execute(&self, input: &str, state: &mut AppState, config: &Config) -> CommandResult {
        let input = input.trim();
        if !input.starts_with('/') {
            return CommandResult::NeedsAgent(input.to_string());
        }

        let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
        let name = parts[0];
        let args = parts.get(1).unwrap_or(&"");

        match self.commands.get(name) {
            Some(cmd) => cmd.execute(args, state, config),
            None => CommandResult::Error(format!("Unknown command: /{name}")),
        }
    }

    pub fn completions(&self) -> Vec<String> {
        self.commands.keys().map(|k| format!("/{k}")).collect()
    }

    pub fn suggest(&self, query: &str) -> Vec<CommandSuggestion> {
        let q = query.trim_start_matches('/').to_ascii_lowercase();
        let mut out: Vec<CommandSuggestion> = self
            .commands
            .values()
            .map(|c| CommandSuggestion {
                name: c.name().to_string(),
                description: c.description().to_string(),
                usage: c.usage().to_string(),
            })
            .filter(|s| q.is_empty() || s.name.to_ascii_lowercase().starts_with(&q))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}
