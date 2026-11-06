//! `pooprusteek trust` — подтвердить проектный конфиг, который хочет изменить, куда уходят запросы.
//!
//! Проект может предложить аккаунты, маршруты, роли, включение шлюза и серверы MCP
//! (`mcp.config.json`, `.vscode/mcp.json`), но до подтверждения действуют общие значения,
//! а серверы проекта не запускаются. Подтверждение — хэш содержимого файла, записанный в общий конфиг:
//! файл изменился — спросим снова. Ключи это не открывает: хост секрета подтверждается отдельно
//! (`pooprusteek secrets bind`).

use crate::config::{self, Config, LayerStatus};
use clap::Args;
use std::io::{IsTerminal, Write};

#[derive(Args, Debug)]
pub struct TrustArgs {
    /// Skip the question after you have read what is listed (needs a real terminal).
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
    let layer = config
        .source
        .layers
        .iter()
        .find(|l| l.layer == config::Layer::Project);
    if args.revoke {
        return revoke(&config, layer);
    }
    if let Some(LayerStatus::Rejected(reasons)) = layer.map(|l| &l.status) {
        println!(
            "{} is ignored as a whole and cannot be confirmed:",
            layer.map_or_else(String::new, |l| l.path.display().to_string())
        );
        for reason in reasons {
            println!("  - {reason}");
        }
    }
    let requests = config.pending_trust_requests();
    if requests.is_empty() {
        println!(
            "Nothing in this project needs confirmation (no project config or MCP files, or already confirmed)."
        );
        return i32::from(matches!(
            layer.map(|l| &l.status),
            Some(LayerStatus::Rejected(_))
        ));
    }
    for request in &requests {
        println!(
            "{} wants to change where requests go or what runs:",
            request.config_path.display()
        );
        for line in &request.held_back {
            println!("  - {line}");
        }
        println!("  Content hash: {}", &request.hash[..16]);
    }
    println!(
        "Until you confirm, your global settings stay in force and project MCP servers do not start. \
         Keys are not released by this: for an account that needs one, run `pooprusteek secrets bind <ref> <host>`."
    );
    match confirm(
        &requests,
        args.yes,
        &crate::secrets::human::require_human,
        &ask,
    ) {
        Ok(()) => {
            println!("Confirmed. If a file changes, you will be asked again.");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// Снять подтверждение со всего проекта: конфиг и каждый файл MCP.
fn revoke(config: &Config, layer: Option<&config::LayerInfo>) -> i32 {
    let mut keys: Vec<(String, std::path::PathBuf)> = layer
        .map(|l| (config::project_key(&l.path), l.path.clone()))
        .into_iter()
        .collect();
    keys.extend(
        config
            .source
            .project_mcp
            .iter()
            .map(|f| (f.request.key.clone(), f.request.config_path.clone())),
    );
    if keys.is_empty() {
        println!("No project config or MCP files apply here, nothing to withdraw.");
        return 0;
    }
    let mut code = 0;
    for (key, path) in keys {
        match config::untrust_project(&key) {
            Ok(()) => println!("Confirmation withdrawn for {}.", path.display()),
            Err(error) => {
                eprintln!("Error: {error}");
                code = 1;
            }
        }
    }
    code
}

/// Подтверждение — только человек за терминалом; `--yes` лишь пропускает вопрос.
fn confirm(
    requests: &[config::TrustRequest],
    yes: bool,
    gate: &dyn Fn(&str) -> Result<(), String>,
    question: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    gate("pooprusteek trust").map_err(|message| format!("Error: {message}"))?;
    if !(yes || question("Trust exactly these files?")) {
        return Err("Not confirmed. Nothing was changed.".to_string());
    }
    for request in requests {
        config::trust_project(request).map_err(|error| format!("Error: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tag: &str) -> config::TrustRequest {
        config::TrustRequest {
            config_path: std::path::PathBuf::from(format!("/m2-{tag}/.pooprusteek/config.toml")),
            key: format!("/m2-{tag}/.pooprusteek/config.toml"),
            hash: "0".repeat(64),
            held_back: Vec::new(),
            summary: config::GatedSummary::default(),
        }
    }

    fn written(tag: &str) -> bool {
        std::fs::read_to_string(config::Config::path())
            .is_ok_and(|text| text.contains(&format!("m2-{tag}")))
    }

    #[test]
    fn yes_does_not_bypass_the_human_gate() {
        let refuse = |what: &str| Err(format!("{what}: no terminal"));
        let result = confirm(&[request("refused")], true, &refuse, &|_| true);
        assert!(result.unwrap_err().contains("no terminal"));
        assert!(!written("refused"), "nothing may reach the config");
    }

    #[test]
    fn a_person_with_yes_is_not_asked_and_confirms() {
        let allow = |_: &str| Ok(());
        let never_asked = |_: &str| -> bool { panic!("--yes must skip the question") };
        confirm(&[request("allowed")], true, &allow, &never_asked).unwrap();
        assert!(written("allowed"));
    }

    #[test]
    fn a_person_who_declines_changes_nothing() {
        let allow = |_: &str| Ok(());
        let result = confirm(&[request("declined")], false, &allow, &|_| false);
        assert!(result.unwrap_err().contains("Nothing was changed"));
        assert!(!written("declined"));
    }
}
