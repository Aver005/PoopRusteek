//! `pooprusteek secrets` — задать, удалить, посмотреть секреты, не показывая значений.
//!
//! Значение никогда не идёт через аргументы командной строки: `ps` показывает argv.
//! `set`, `rm` и `bind` выполняет только человек за терминалом: канал и запуск из инструмента отказывают.

use crate::secrets::{Secret, ops, process_store};
use clap::{Args, Subcommand};
use std::io::Write;

#[derive(Args, Debug)]
pub struct SecretsArgs {
    #[command(subcommand)]
    pub action: SecretsAction,
}

#[derive(Subcommand, Debug)]
pub enum SecretsAction {
    /// Names, references and state of every secret the config uses (no values).
    List,
    /// Store a secret: reads the value from a hidden prompt (needs a real terminal).
    Set {
        /// Account or provider name (`openai`), or an entry like `global/team-key`.
        name: String,
    },
    /// Delete a secret from the keyring.
    Rm { name: String },
    /// Check that the keyring works: write, read back, delete (including a long value).
    Test,
    /// Allow a secret reference to be used with a host (for project-config accounts).
    Bind {
        /// Reference like `keyring:global/openai` or `env:OPENAI_API_KEY`.
        reference: String,
        host: String,
    },
}

impl SecretsAction {
    /// Название действия, если оно меняет секреты или привязки и потому требует человека.
    fn human_only(&self) -> Option<&'static str> {
        match self {
            Self::Set { .. } => Some("pooprusteek secrets set"),
            Self::Rm { .. } => Some("pooprusteek secrets rm"),
            Self::Bind { .. } => Some("pooprusteek secrets bind"),
            Self::List | Self::Test => None,
        }
    }
}

/// Читает значение со скрытого ввода; терминал уже проверен `require_human`.
fn read_value(prompt: &str) -> Result<Secret, String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    enable_raw_mode().map_err(|e| format!("cannot read the terminal: {e}"))?;
    let mut value = String::new();
    let outcome = loop {
        match read() {
            Ok(Event::Key(key))
                if key.kind != KeyEventKind::Release
                    && crate::clipboard::is_paste_shortcut(&key) =>
            {
                match crate::clipboard::read() {
                    Ok(text) => value.push_str(text.trim()),
                    Err(e) => break Err(format!("cannot read the clipboard: {e}")),
                }
            }
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Esc => break Err("cancelled".to_string()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("cancelled".to_string());
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) => value.push(c),
                _ => {}
            },
            Ok(Event::Paste(text)) => value.push_str(text.trim()),
            Ok(_) => {}
            Err(e) => break Err(format!("terminal error: {e}")),
        }
    };
    let _ = disable_raw_mode();
    eprintln!();
    outcome.map(|()| Secret::new(value.trim()))
}

/// Возвращает код выхода процесса.
pub fn run(args: SecretsArgs) -> i32 {
    run_with(args, &crate::secrets::human::require_human)
}

fn run_with(args: SecretsArgs, gate: &dyn Fn(&str) -> Result<(), String>) -> i32 {
    if let Some(what) = args.action.human_only()
        && let Err(message) = gate(what)
    {
        eprintln!("Error: {message}");
        return 1;
    }
    let store = process_store();
    let result: Result<Vec<String>, String> = match args.action {
        SecretsAction::List => {
            let rows = ops::list(&store);
            Ok(if rows.is_empty() {
                vec!["No secrets are referenced by the config.".to_string()]
            } else {
                rows.iter().map(ops::Row::line).collect()
            })
        }
        SecretsAction::Set { name } => ops::entry_id(&name)
            .and_then(|_| read_value("Secret (input is hidden): "))
            .and_then(|value| ops::set(&store, &name, value))
            .map(|m| vec![m]),
        SecretsAction::Rm { name } => ops::remove(&store, &name).map(|m| vec![m]),
        SecretsAction::Test => Ok(ops::selftest(&store)),
        SecretsAction::Bind { reference, host } => {
            crate::config::bind_secret_host(&reference, &host)
                .map(|()| vec![format!("{reference} may now be used with {host}.")])
                .map_err(|e| e.to_string())
        }
    };
    match result {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(message) => {
            eprintln!("Error: {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refuse(what: &str) -> Result<(), String> {
        Err(format!("{what}: no terminal"))
    }

    #[test]
    fn bind_without_a_person_writes_no_binding() {
        let code = run_with(
            SecretsArgs {
                action: SecretsAction::Bind {
                    reference: "env:POOP_M2_CANARY".into(),
                    host: "m2-attacker.example".into(),
                },
            },
            &refuse,
        );
        assert_eq!(code, 1);
        let text = std::fs::read_to_string(crate::config::Config::path()).unwrap_or_default();
        assert!(!text.contains("m2-attacker.example"), "{text}");
    }

    #[test]
    fn set_and_rm_are_refused_before_touching_input_or_store() {
        for action in [
            SecretsAction::Set { name: "m2".into() },
            SecretsAction::Rm { name: "m2".into() },
        ] {
            assert_eq!(run_with(SecretsArgs { action }, &refuse), 1);
        }
    }

    #[test]
    fn only_reading_actions_skip_the_gate() {
        assert!(SecretsAction::List.human_only().is_none());
        assert!(SecretsAction::Test.human_only().is_none());
        let bind = SecretsAction::Bind {
            reference: "env:X".into(),
            host: "h".into(),
        };
        assert!(bind.human_only().is_some());
    }
}
