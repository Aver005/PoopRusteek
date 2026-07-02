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

pub enum JobCommandAction {
    List,
    Kill(u64),
    Prune,
}

pub enum CommandResult {
    Handled,
    LoadSession(String),
    ResetProvider,
    Quit,
    Error(String),
    TtlUpdate(u64),
    ReloadMcp,
    ShowTools,
    Jobs(JobCommandAction),
    OpenWhitelist,
    ShowSkills,
    ToggleSkill(String, bool),
    /// `/btw <question>` — run a one-shot side-answer in the background.
    Sidechat(String),
    /// `/new` — open a fresh parallel chat and focus it.
    NewChat,
    /// `/chats` — open the chat switcher picker.
    OpenChats,
    /// `/agent <prompt>` — launch a background sub-agent.
    SpawnAgent(String),
    /// `/agents` — open the running-agents picker (to stop them).
    OpenAgents,
    /// `/delete` / `/delete-local` — open the session-deletion picker (or,
    /// with an explicit id, jump straight to its confirmation step).
    OpenDeleteSessions {
        scope: crate::app::events::SessionScope,
        session_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct CommandSuggestion {
    pub name: String,
    pub description: String,
    pub usage: String,
}

/// A single command's metadata as shown by `/help`. Separate from
/// `CommandSuggestion` even though the shape matches — this one is the
/// contract with `HelpCommand`, that one is the contract with autocomplete;
/// keeping them distinct lets either evolve independently.
#[derive(Debug, Clone)]
pub struct HelpEntry {
    pub name: String,
    pub description: String,
    pub usage: String,
}

/// Parse a raw input line into `(command_name, args)`. Returns `None` if the
/// input isn't a slash command (doesn't start with `/`), in which case the
/// caller should treat it as a prompt for the agent. The name is always
/// returned bare (no leading `/`) to match `Command::name()` and the
/// registry's lookup key.
fn parse_input(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    let mut parts = rest.splitn(2, ' ');
    let name = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("");
    Some((name, args))
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
        self.register(Box::new(defs::attach::AttachCommand));
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
        self.register(Box::new(defs::export::ExportCommand));
        self.register(Box::new(defs::goal::GoalCommand));
        self.register(Box::new(defs::import::ImportCommand));
        self.register(Box::new(defs::ps::PsCommand));
        self.register(Box::new(defs::jobs::JobsCommand));
        self.register(Box::new(defs::btw::BtwCommand));
        self.register(Box::new(defs::chats::NewChatCommand));
        self.register(Box::new(defs::chats::ChatsCommand));
        self.register(Box::new(defs::agent::AgentCommand));
        self.register(Box::new(defs::agent::AgentsCommand));
        self.register(Box::new(defs::delete::DeleteCommand));
        self.register(Box::new(defs::delete::DeleteLocalCommand));

        // Registered last so its own entry is included in the generated list.
        let help = Box::new(defs::help::HelpCommand::new(self.help_entries()));
        self.register(help);
    }

    /// Snapshot of `(name, description, usage)` for every command registered
    /// so far, sorted by name. Used to build `/help` from the live registry
    /// instead of a hand-maintained list that drifts out of sync.
    fn help_entries(&self) -> Vec<HelpEntry> {
        let mut entries: Vec<HelpEntry> = self
            .commands
            .values()
            .map(|c| HelpEntry {
                name: c.name().to_string(),
                description: c.description().to_string(),
                usage: c.usage().to_string(),
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    pub fn register(&mut self, cmd: Box<dyn Command>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    pub fn execute(&self, input: &str, state: &mut AppState, config: &Config) -> CommandResult {
        let input = input.trim();
        let Some((name, args)) = parse_input(input) else {
            return CommandResult::Error(format!("Not a command: {input}"));
        };

        match self.commands.get(name) {
            Some(cmd) => cmd.execute(args, state, config),
            None => CommandResult::Error(format!("Unknown command: /{name}")),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_strips_leading_slash_and_splits_args() {
        assert_eq!(parse_input("/goal"), Some(("goal", "")));
        assert_eq!(parse_input("/load abc123"), Some(("load", "abc123")));
        assert_eq!(
            parse_input("/attach a.txt b.txt"),
            Some(("attach", "a.txt b.txt"))
        );
    }

    #[test]
    fn parse_input_rejects_non_slash_input() {
        assert_eq!(parse_input("hello there"), None);
        assert_eq!(parse_input(""), None);
    }

    /// (a) After `register_defaults`, no registered command name starts with
    /// `/`. The registry key is always the bare name — a leading slash here
    /// would make the command unreachable via `execute`, exactly like the
    /// `/goal` regression this test guards against.
    #[test]
    fn no_registered_command_name_starts_with_slash() {
        let registry = CommandRegistry::new();
        for name in registry.commands.keys() {
            assert!(
                !name.starts_with('/'),
                "command name {name:?} must not start with '/'"
            );
        }
    }

    /// (b) For every registered name, dispatching `"/{name}"` through the same
    /// parse path `execute` uses resolves to a real command in the registry.
    #[test]
    fn every_registered_command_is_reachable_via_dispatch_parse() {
        let registry = CommandRegistry::new();
        let names: Vec<String> = registry.commands.keys().cloned().collect();
        assert!(!names.is_empty(), "registry should not be empty");

        for name in names {
            let input = format!("/{name}");
            let (parsed_name, _args) = parse_input(&input)
                .unwrap_or_else(|| panic!("{input:?} should parse as a command"));
            assert!(
                registry.commands.contains_key(parsed_name),
                "parsed name {parsed_name:?} from {input:?} should resolve in the registry"
            );
        }
    }

    /// (c) `suggest("")` lists every registered command.
    #[test]
    fn suggest_empty_query_returns_every_registered_command() {
        let registry = CommandRegistry::new();
        let suggestions = registry.suggest("");
        assert_eq!(suggestions.len(), registry.commands.len());

        let suggested_names: std::collections::HashSet<&str> =
            suggestions.iter().map(|s| s.name.as_str()).collect();
        for name in registry.commands.keys() {
            assert!(
                suggested_names.contains(name.as_str()),
                "suggest(\"\") is missing registered command {name:?}"
            );
        }
    }

    /// `/help`'s entries are built from `help_entries()` before `HelpCommand`
    /// is constructed, so this checks the same snapshot every registered
    /// command name ends up in the generated help text.
    #[test]
    fn help_entries_mention_every_registered_command() {
        let registry = CommandRegistry::new();
        let entries = registry.help_entries();
        let entry_names: std::collections::HashSet<&str> =
            entries.iter().map(|e| e.name.as_str()).collect();

        for name in registry.commands.keys() {
            assert!(
                entry_names.contains(name.as_str()),
                "help_entries() is missing registered command {name:?}"
            );
        }
    }
}
