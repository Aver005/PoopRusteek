//! `pooprusteek trust` — подтвердить проектный конфиг, который хочет изменить, куда уходят запросы.
//!
//! Проект может предложить аккаунты, маршруты, роли и включение шлюза, но до подтверждения
//! действуют общие значения. Подтверждение — хэш содержимого файла, записанный в общий конфиг:
//! файл изменился — спросим снова. Ключи это не открывает: хост секрета подтверждается отдельно
//! (`pooprusteek secrets bind`).

use crate::config::{self, Config, LayerStatus};
use clap::Args;
use std::io::{IsTerminal, Write};

#[derive(Args, Debug)]
pub struct TrustArgs {
    /// Confirm without asking (after you have read what is listed).
    #[arg(long)]
    pub yes: bool,
    /// Take the confirmation back for this project's config.
    #[arg(long)]
    pub revoke: bool,
}

fn ask(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [type yes] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("yes")
}

/// Возвращает код выхода процесса.
pub fn run(args: TrustArgs, explicit_config: Option<&std::path::Path>) -> i32 {
    let config: Config = match config::load_from_optional(explicit_config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Error: the config could not be loaded: {error}");
            return 1;
        }
    };
    let Some(layer) = config
        .source
        .layers
        .iter()
        .find(|l| l.layer == config::Layer::Project)
    else {
        println!("No project config (.pooprusteek/config.toml) applies here, nothing to confirm.");
        return 0;
    };
    if args.revoke {
        let key = config::project_key(&layer.path);
        return match config::untrust_project(&key) {
            Ok(()) => {
                println!("Confirmation withdrawn for {}.", layer.path.display());
                0
            }
            Err(error) => {
                eprintln!("Error: {error}");
                1
            }
        };
    }
    if let LayerStatus::Rejected(reasons) = &layer.status {
        println!(
            "{} is ignored as a whole and cannot be confirmed:",
            layer.path.display()
        );
        for reason in reasons {
            println!("  - {reason}");
        }
        return 1;
    }
    let Some(request) = &config.source.pending_trust else {
        println!(
            "{} proposes nothing that needs confirmation (or is already confirmed).",
            layer.path.display()
        );
        return 0;
    };
    println!(
        "{} wants to change where requests go:",
        request.config_path.display()
    );
    for line in &request.held_back {
        println!("  - {line}");
    }
    println!(
        "Content hash: {}. Until you confirm, your global settings stay in force. Keys are not \
         released by this: for an account that needs one, run `pooprusteek secrets bind <ref> <host>`.",
        &request.hash[..16]
    );
    if !(args.yes || ask("Trust exactly this file?")) {
        eprintln!("Not confirmed. Nothing was changed.");
        return 1;
    }
    match config::trust_project(request) {
        Ok(()) => {
            println!("Confirmed. If the file changes, you will be asked again.");
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}
