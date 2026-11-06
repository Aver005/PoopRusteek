//! `pooprusteek secrets` — задать, удалить, посмотреть секреты, не показывая значений.
//!
//! Значение никогда не идёт через аргументы командной строки: `ps` показывает argv.
//! Оно читается из stdin (канал) или со скрытого ввода в терминале.

use crate::secrets::{Secret, ops, process_store};
use clap::{Args, Subcommand};
use std::io::{IsTerminal, Read, Write};

#[derive(Args, Debug)]
pub struct SecretsArgs {
    #[command(subcommand)]
    pub action: SecretsAction,
}

#[derive(Subcommand, Debug)]
pub enum SecretsAction {
    /// Names, references and state of every secret the config uses (no values).
    List,
    /// Store a secret: reads the value from stdin or a hidden prompt.
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

/// Читает значение без эха. В канале — одна строка stdin; в терминале — скрытый ввод.
fn read_value(prompt: &str) -> Result<Secret, String> {
    if !std::io::stdin().is_terminal() {
        let mut text = String::new();
        std::io::stdin()
            .take(70_000)
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read stdin: {}", e.kind()))?;
        return Ok(Secret::new(text.lines().next().unwrap_or("").trim()));
    }
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    enable_raw_mode().map_err(|e| format!("cannot read the terminal: {e}"))?;
    let mut value = String::new();
    let outcome = loop {
        match read() {
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
