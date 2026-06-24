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
    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult;
}

pub enum CommandResult {
    Handled,
    NeedsAgent(String),
    Error(String),
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
        self.register(Box::new(defs::clear::ClearCommand));
        self.register(Box::new(defs::quit::QuitCommand));
        self.register(Box::new(defs::version::VersionCommand));
        self.register(Box::new(defs::compact::CompactCommand));
        self.register(Box::new(defs::reset::ResetCommand));
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
}
