//! Команды оболочки, которые трогают доверие, привязки секретов и общий конфиг.
//!
//! Белый список на них не действует: человек видит окно с причиной каждый раз.
//! Честно: разбор простой, а `bash` может править файл напрямую — это смягчение, не запрет.

use serde_json::Value;

/// Что показывать в окне подтверждения.
pub const REASON: &str =
    "risky: touches trust, secret bindings or the global config (the whitelist does not apply)";

/// Причина, по которой команду `bash`/`powershell` всегда решает человек, если она есть.
pub fn sensitive_shell_reason(tool: &str, arguments: &Value) -> Option<&'static str> {
    if !matches!(tool, "bash" | "powershell") {
        return None;
    }
    let command = arguments["command"].as_str()?;
    touches_trust_or_secrets(command).then_some(REASON)
}

/// Нижний регистр, косые вперёд, без кавычек и экранирования: `po"op"rusteek` читается как есть.
fn normalize(command: &str) -> String {
    command
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '`' | '^'))
        .map(|c| if c == '\\' { '/' } else { c })
        .collect::<String>()
        .to_lowercase()
}

/// Путь папки в той же форме, что и `normalize`, без хвостового слэша.
fn normalized_dir(path: &std::path::Path) -> String {
    normalize(&path.to_string_lossy())
        .trim_end_matches('/')
        .to_string()
}

fn is_program(token: &str) -> bool {
    let last = token.rsplit('/').next().unwrap_or(token);
    matches!(last, "pooprusteek" | "pooprusteek.exe")
}

fn touches_trust_or_secrets(command: &str) -> bool {
    let text = normalize(command);
    if text.contains(".pooprusteek") || text.contains("pooprusteek/config.toml") {
        return true;
    }
    // Настоящие папки конфига: новая и прежняя.
    let places = [
        Some(crate::config::global_dir()),
        crate::config::legacy_config_file()
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf),
    ];
    if places
        .iter()
        .flatten()
        .map(|dir| normalized_dir(dir))
        .any(|dir| !dir.is_empty() && text.contains(&dir))
    {
        return true;
    }
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || ";&|(){}<>=,".contains(c))
        .filter(|t| !t.is_empty())
        .collect();
    let runs_us = tokens.iter().any(|t| is_program(t))
        || (tokens.contains(&"cargo") && tokens.contains(&"run"));
    runs_us && tokens.iter().any(|t| matches!(*t, "trust" | "secrets"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hit(command: &str) -> bool {
        sensitive_shell_reason("bash", &json!({ "command": command })).is_some()
    }

    #[test]
    fn trust_and_secret_commands_are_flagged_however_they_are_spelled() {
        for command in [
            "pooprusteek trust --yes",
            "  pooprusteek    secrets   bind keyring:global/x evil.example",
            "./target/debug/pooprusteek secrets rm openai",
            "C:\\tools\\pooprusteek.exe trust",
            "\"pooprusteek\" 'trust' --yes",
            "po\"oprus\"teek trust --yes",
            "POOPRUSTEEK Secrets set x",
            "cd repo && pooprusteek --config c.toml trust --yes",
            "cargo run -- trust --yes",
            "echo 1 | pooprusteek secrets set openai",
            "(pooprusteek trust)",
        ] {
            assert!(hit(command), "must be flagged: {command}");
        }
    }

    #[test]
    fn configs_and_the_project_layer_are_flagged() {
        for command in [
            "cat .pooprusteek/config.toml",
            "echo x >> ~/.pooprusteek/config.toml",
            "sed -i s/a/b/ .pooprusteek\\config.toml",
            "type %APPDATA%\\pooprusteek\\config.toml",
            "cp evil .config/pooprusteek/config.toml",
        ] {
            assert!(hit(command), "must be flagged: {command}");
        }
        let global = crate::config::global_dir();
        assert!(hit(&format!(
            "echo x > {}",
            global.join("config.toml").display()
        )));
    }

    #[test]
    fn ordinary_commands_are_left_alone() {
        for command in [
            "cargo test --bin pooprusteek",
            "git status",
            "grep -rn trust src",
            "ls -la",
            "echo secrets are fun",
            "cargo build --release",
        ] {
            assert!(!hit(command), "must not be flagged: {command}");
        }
    }

    #[test]
    fn other_tools_and_missing_commands_are_not_flagged() {
        assert!(
            sensitive_shell_reason("write", &json!({ "command": "pooprusteek trust" })).is_none()
        );
        assert!(sensitive_shell_reason("bash", &json!({})).is_none());
    }
}
