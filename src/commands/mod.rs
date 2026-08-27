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

/// `/rag` subcommand intents — interpreted in `apply_command_result`,
/// which has the `App`-level access (semantic service, mutable config)
/// these effects need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RagAction {
    /// `/rag` — short how-it-works + live status + subcommand list.
    Status,
    /// `/rag on` / `/rag off` — persist the flag and flip the service.
    SetEnabled(bool),
    /// `/rag reload` — re-verify the model (re-download missing files)
    /// and re-embed every corpus.
    Reload,
    /// `/rag-limit` — show the embedder batch cap, its resolved value, and
    /// detected RAM.
    LimitStatus,
    /// `/rag-limit <n|auto|off>` — set the embedder batch cap (memory guard).
    SetLimit(crate::config::RagLimit),
}

/// `/serve` + `/server` intents — interpreted in `app/serve.rs`, which
/// owns the server handle and the mutable config these effects need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeAction {
    /// `/serve` — running state, address, counters, exposed models.
    Status,
    /// `/serve on` — launch the API server.
    Start,
    /// `/serve off` — shut the API server down.
    Stop,
    /// `/server <port>` — persist the port; restarts a running server.
    SetPort(u16),
    /// `/serve api <dialect>` — persist the wire dialect; restarts a
    /// running server.
    SetApi(crate::config::ServerApi),
}

/// `/update` + `/autoupdate` intents — interpreted in
/// `apply_update_action` (app::keys::dispatch), which owns the in-flight
/// guard, the event channel, and the mutable config these effects need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// `/update` — check the `latest` release, install on hash mismatch.
    Run,
    /// `/autoupdate` — show whether the startup check is enabled.
    AutoStatus,
    /// `/autoupdate on` / `/autoupdate off` — persist the flag.
    SetAuto(bool),
}

/// `/refetch-providers` + `/cache-providers` intents — interpreted in
/// `app/providers.rs::apply_provider_models_action`, which owns the model
/// cache handle and mutable config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderModelsAction {
    /// Bare command — current periods + per-provider fetch state.
    Show,
    /// `/refetch-providers <ms>` — background refetch period (0 = off).
    SetRefetch(u64),
    /// `/cache-providers <ms>` — persisted-cache validity (0 = off).
    SetCache(u64),
}

pub enum CommandResult {
    Handled,
    LoadSession(String),
    ResetProvider,
    Quit,
    Error(String),
    TtlUpdate(u64),
    ReloadMcp,
    /// `/mcp auth` / `/mcp oauth` — open the authorization picker (servers
    /// currently `AuthRequired`).
    OpenMcpAuth,
    /// `/mcp add [args]` — with no args, opens the method-choice modal
    /// (paste JSON vs. step-by-step wizard); with args, the handler tries
    /// `mcp_add::parse_quick_add` first and falls back to the same
    /// method-choice modal on any parse failure.
    OpenMcpAdd(Option<String>),
    ShowTools,
    Jobs(JobCommandAction),
    OpenWhitelist,
    ShowSkills,
    ToggleSkill(String, bool),
    /// `/btw <question>` — run a one-shot side-answer in the background.
    Sidechat(String),
    /// `/compact [1|2|3]` — run the compaction ladder by hand. `Some(mode)`
    /// sets the focused chat's mode as well as running with it; `None` runs
    /// with whatever the chat already has (falling back to
    /// `[context] compact_mode`). Summarising needs a model call, which a
    /// synchronous command cannot make — the work belongs to the interpreter.
    Compact(Option<u8>),
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
    /// `/logout` and `/wipe` — open the generic confirmation modal.
    OpenConfirm(crate::app::events::ConfirmAction),
    /// `/providers` — open the provider-management panel.
    OpenProviders,
    /// `/providers add [args]` — with no args, open the step-by-step
    /// wizard; with args, the handler tries the quick
    /// `<name> <base_url> [model] [api_key]` form and falls back to the
    /// wizard on parse failure.
    OpenProviderAdd(Option<String>),
    /// `/models` — fetch the active provider's models and open the picker.
    OpenModels,
    /// `/models <id>` — validate the id against the provider's model list
    /// and switch to it (404-style error when it doesn't exist).
    SwitchModel(String),
    /// `/rag [on|off|reload]` — semantic-matching control.
    Rag(RagAction),
    /// `/search <query>` — history search; results flush back as a
    /// UI-only message once the off-loop lookup completes.
    SearchHistory(String),
    /// `/themes` — open the theme gallery (live preview, instant apply).
    OpenThemes,
    /// `/themes new` — open the step-by-step theme-creation wizard.
    OpenThemeWizard,
    /// `/themes <name>` — validate the name against presets + custom
    /// themes and switch to it.
    SetTheme(String),
    /// `/serve [on|off|api <dialect>]` and `/server <port>` — API-server
    /// control.
    Serve(ServeAction),
    /// `/refetch-providers` + `/cache-providers` — provider model-list
    /// freshness knobs.
    ProviderModels(ProviderModelsAction),
    /// `/update` + `/autoupdate` — self-update control.
    Update(UpdateAction),
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

/// Run `body` with the trimmed, non-empty `args`. If the command was invoked
/// without an argument, return the canonical `Usage: {usage}` error instead —
/// the shared prologue for every command that requires one.
fn with_args(args: &str, usage: &str, body: impl FnOnce(&str) -> CommandResult) -> CommandResult {
    let args = args.trim();
    if args.is_empty() {
        return CommandResult::Error(format!("Usage: {usage}"));
    }
    body(args)
}

/// Persist `config`, then run `then` for the success result. `then` only runs
/// after the save landed, so success-only side effects (confirmation messages,
/// …) stay gated behind it; a failed write becomes the canonical
/// "Failed to save config" error.
fn save_config_then(config: &Config, then: impl FnOnce() -> CommandResult) -> CommandResult {
    match crate::config::save_or_message(config) {
        Ok(()) => then(),
        Err(message) => CommandResult::Error(message),
    }
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
        self.register(Box::new(defs::default_compact::DefaultCompactCommand));
        self.register(Box::new(defs::last::LastCommand));
        self.register(Box::new(defs::load::LoadCommand));
        self.register(Box::new(defs::session_info::SessionInfoCommand));
        self.register(Box::new(defs::session_list::SessionListCommand));
        self.register(Box::new(defs::reset::ResetCommand));
        self.register(Box::new(defs::rate::RateCommand));
        self.register(Box::new(defs::retry::RetryCommand));
        self.register(Box::new(defs::mcp::McpCommand));
        self.register(Box::new(defs::providers::ProvidersCommand));
        self.register(Box::new(defs::models::ModelsCommand));
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
        self.register(Box::new(defs::logout::LogoutCommand));
        self.register(Box::new(defs::wipe::WipeCommand));
        self.register(Box::new(defs::debug::DebugCommand));
        self.register(Box::new(defs::rag::RagCommand));
        self.register(Box::new(defs::rag_limit::RagLimitCommand));
        self.register(Box::new(defs::search::SearchCommand));
        self.register(Box::new(defs::themes::ThemesCommand));
        self.register(Box::new(defs::serve::ServeCommand));
        self.register(Box::new(defs::serve::ServerCommand));
        self.register(Box::new(defs::provider_models::RefetchProvidersCommand));
        self.register(Box::new(defs::provider_models::CacheProvidersCommand));
        self.register(Box::new(defs::update::UpdateCommand));
        self.register(Box::new(defs::update::AutoUpdateCommand));

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
