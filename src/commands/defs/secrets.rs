use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;
use crate::secrets::{Secret, ops, process_store};

/// `/secrets` — секреты в системном хранилище. Работа с хранилищем идёт вне цикла событий.
pub struct SecretsCommand;

const USAGE: &str = "/secrets [list | set <name> | rm <name> | test | bind <ref> <host>]";

/// Запись и удаление секретов, привязка хоста — только у человека: из инструмента агента отказ.
fn human_only_refusal(action: &str, agent_child: bool) -> Option<CommandResult> {
    let changes = matches!(action, "set" | "rm" | "remove" | "delete" | "bind");
    (changes && agent_child).then(|| {
        CommandResult::Error(
            "/secrets set, rm and bind change keys and trust: this screen was started from an agent tool, so they are off. Run pooprusteek in your own terminal."
                .to_string(),
        )
    })
}

impl Command for SecretsCommand {
    fn name(&self) -> &str {
        "secrets"
    }

    fn description(&self) -> &str {
        "Keys in the system keyring (values are never shown)"
    }

    fn usage(&self) -> &str {
        USAGE
    }

    fn execute(&self, args: &str, _state: &mut AppState, _config: &Config) -> CommandResult {
        let args = args.trim();
        let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let rest = rest.trim();
        if let Some(refusal) = human_only_refusal(action, crate::secrets::human::is_agent_child()) {
            return refusal;
        }
        match action {
            "" | "list" => CommandResult::Detached(Box::new(|| {
                let rows = ops::list(&process_store());
                if rows.is_empty() {
                    "No secrets are referenced by the config.".to_string()
                } else {
                    rows.iter()
                        .map(ops::Row::line)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })),
            "set" => {
                let (name, value) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if !name.is_empty() && value.trim().is_empty() {
                    // Без значения в строке: оно вводится в модалке под маской.
                    return CommandResult::PromptSecret(name.to_string());
                }
                if name.is_empty() {
                    return CommandResult::Error(
                        "Usage: /secrets set <name> — the value is asked in a hidden prompt. This line is not saved to history."
                            .to_string(),
                    );
                }
                let (name, value) = (name.to_string(), Secret::new(value.trim()));
                CommandResult::Detached(Box::new(move || {
                    ops::set(&process_store(), &name, value)
                        .unwrap_or_else(|e| format!("Error: {e}"))
                }))
            }
            "rm" | "remove" | "delete" if !rest.is_empty() => {
                let name = rest.to_string();
                CommandResult::Detached(Box::new(move || {
                    ops::remove(&process_store(), &name).unwrap_or_else(|e| format!("Error: {e}"))
                }))
            }
            "test" => {
                CommandResult::Detached(Box::new(|| ops::selftest(&process_store()).join("\n")))
            }
            "bind" => {
                let mut parts = rest.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some(reference), Some(host)) => {
                        let (reference, host) = (reference.to_string(), host.to_string());
                        CommandResult::Detached(Box::new(move || {
                            match crate::config::bind_secret_host(&reference, &host) {
                                Ok(()) => format!("{reference} may now be used with {host}."),
                                Err(e) => format!("Error: {e}"),
                            }
                        }))
                    }
                    _ => {
                        CommandResult::Error("Usage: /secrets bind <reference> <host>".to_string())
                    }
                }
            }
            _ => CommandResult::Error(format!("Usage: {USAGE}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_child_cannot_change_keys_or_trust_but_can_look() {
        for action in ["set", "rm", "remove", "delete", "bind"] {
            assert!(
                matches!(
                    human_only_refusal(action, true),
                    Some(CommandResult::Error(_))
                ),
                "{action}"
            );
            assert!(
                human_only_refusal(action, false).is_none(),
                "a person may {action}"
            );
        }
        for action in ["", "list", "test"] {
            assert!(
                human_only_refusal(action, true).is_none(),
                "{action} only reads"
            );
        }
    }
}
