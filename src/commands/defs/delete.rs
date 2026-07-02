use crate::app::events::SessionScope;
use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/delete [session_id]` — delete sessions everywhere they exist (the
/// DeepSeek account copy and the local file). Without an id it opens the
/// shared deletion picker with the filter preset to `All`.
pub struct DeleteCommand;

/// `/delete-local [session_id]` — same picker, filter preset to `Local`:
/// only on-disk session files are targeted, account copies stay.
pub struct DeleteLocalCommand;

fn parse(args: &str, scope: SessionScope) -> CommandResult {
    let id = args.trim();
    CommandResult::OpenDeleteSessions {
        scope,
        session_id: if id.is_empty() { None } else { Some(id.to_string()) },
    }
}

impl Command for DeleteCommand {
    fn name(&self) -> &str {
        "delete"
    }

    fn description(&self) -> &str {
        "Delete sessions — remote (DeepSeek account) and local copies"
    }

    fn usage(&self) -> &str {
        "/delete [session_id]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse(args, SessionScope::All)
    }
}

impl Command for DeleteLocalCommand {
    fn name(&self) -> &str {
        "delete-local"
    }

    fn description(&self) -> &str {
        "Delete only locally stored session files (account copies stay)"
    }

    fn usage(&self) -> &str {
        "/delete-local [session_id]"
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        parse(args, SessionScope::Local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_variants_preset_the_right_scope() {
        match parse("", SessionScope::All) {
            CommandResult::OpenDeleteSessions { scope, session_id } => {
                assert_eq!(scope, SessionScope::All);
                assert!(session_id.is_none());
            }
            _ => panic!("expected OpenDeleteSessions"),
        }
        match parse("  abc-123  ", SessionScope::Local) {
            CommandResult::OpenDeleteSessions { scope, session_id } => {
                assert_eq!(scope, SessionScope::Local);
                assert_eq!(session_id.as_deref(), Some("abc-123"));
            }
            _ => panic!("expected OpenDeleteSessions"),
        }
    }
}
