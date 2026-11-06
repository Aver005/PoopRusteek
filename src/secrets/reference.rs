//! Разбор ссылки на секрет: `keyring:<id>`, `env:NAME`, `cmd:[…]`, `file:<путь>`, `none`.
//! Строка-обёртка `gateway::types::SecretRef` проверяет форму, здесь — смысл.

use crate::gateway::types::SecretRef;
use std::path::PathBuf;

/// Куда ссылка ведёт.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefKind {
    Keyring(String),
    Env(String),
    Command(Vec<String>),
    File(PathBuf),
    None,
}

pub const KEYRING_PREFIX: &str = "keyring:";

impl RefKind {
    /// Разбор строки. Значение секрета в сообщение об ошибке не попадает.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw == "none" {
            return Ok(Self::None);
        }
        if let Some(id) = raw.strip_prefix(KEYRING_PREFIX) {
            return valid_key_id(id.trim()).map(|id| Self::Keyring(id.to_string()));
        }
        if let Some(name) = raw.strip_prefix("env:") {
            let name = name.trim();
            let ok = !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            return if ok {
                Ok(Self::Env(name.to_string()))
            } else {
                Err("env: needs a variable name like OPENAI_API_KEY".to_string())
            };
        }
        if let Some(body) = raw.strip_prefix("cmd:") {
            return match serde_json::from_str::<Vec<String>>(body.trim()) {
                Ok(argv) if !argv.is_empty() && !argv[0].trim().is_empty() => {
                    Ok(Self::Command(argv))
                }
                _ => {
                    Err("cmd: needs a JSON array like cmd:[\"op\",\"read\",\"op://x\"]".to_string())
                }
            };
        }
        if let Some(path) = raw.strip_prefix("file:") {
            let path = path.trim();
            return if path.is_empty() {
                Err("file: needs a path".to_string())
            } else {
                Ok(Self::File(crate::util::expand_tilde(path)))
            };
        }
        Err("secret must be none, keyring:<id>, env:NAME, cmd:[…] or file:<path>".to_string())
    }

    pub fn from_ref(reference: &SecretRef) -> Result<Self, String> {
        Self::parse(reference.as_str())
    }

    /// Имя для сообщений: без путей и аргументов команды.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Keyring(_) => "keyring",
            Self::Env(_) => "env",
            Self::Command(_) => "cmd",
            Self::File(_) => "file",
            Self::None => "none",
        }
    }

    /// Имя секрета для сообщений об ошибке.
    pub fn display_name(&self) -> String {
        match self {
            Self::Keyring(id) => format!("keyring:{id}"),
            Self::Env(name) => format!("env:{name}"),
            Self::Command(argv) => format!("cmd:{}", argv.first().map_or("", String::as_str)),
            Self::File(path) => format!("file:{}", path.display()),
            Self::None => "none".to_string(),
        }
    }

    /// Проектный слой недоверенный: исполнять команды и читать файлы по его слову нельзя.
    pub fn allowed_in_project_layer(&self) -> bool {
        matches!(self, Self::Keyring(_) | Self::Env(_) | Self::None)
    }
}

/// Идентификатор записи: `global/deepseek-token`. Без пробелов и управляющих символов.
fn valid_key_id(id: &str) -> Result<&str, String> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('/')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok {
        Ok(id)
    } else {
        Err("keyring: id may use letters, digits, - _ . and / (up to 128 characters)".to_string())
    }
}

/// Ссылка `keyring:<id>` для идентификатора.
pub fn keyring_ref(id: &str) -> String {
    format!("{KEYRING_PREFIX}{id}")
}

/// Это ссылка (любая известная форма) или обычная строка, возможно ключ.
pub fn looks_like_reference(raw: &str) -> bool {
    let raw = raw.trim();
    raw == "none"
        || ["keyring:", "env:", "cmd:", "file:"]
            .iter()
            .any(|p| raw.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_form_parses() {
        assert_eq!(RefKind::parse("none"), Ok(RefKind::None));
        assert_eq!(
            RefKind::parse("keyring:global/deepseek-token"),
            Ok(RefKind::Keyring("global/deepseek-token".into()))
        );
        assert_eq!(
            RefKind::parse("env:OPENAI_API_KEY"),
            Ok(RefKind::Env("OPENAI_API_KEY".into()))
        );
        assert_eq!(
            RefKind::parse(r#"cmd:["op","read","op://x"]"#),
            Ok(RefKind::Command(vec![
                "op".into(),
                "read".into(),
                "op://x".into()
            ]))
        );
        assert!(matches!(
            RefKind::parse("file:/abs/key"),
            Ok(RefKind::File(_))
        ));
    }

    #[test]
    fn a_plain_key_is_not_a_reference_and_is_not_echoed() {
        let error = RefKind::parse("sk-live-abc123").unwrap_err();
        assert!(!error.contains("abc123"), "{error}");
        assert!(!looks_like_reference("sk-live-abc123"));
    }

    #[test]
    fn malformed_references_are_refused() {
        for bad in [
            "keyring:",
            "keyring:../x",
            "keyring:a b",
            "env:",
            "env:1ABC",
            "env:A-B",
            "cmd:op read",
            "cmd:[]",
            "file:",
        ] {
            assert!(RefKind::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_project_layer_may_not_run_commands_or_read_files() {
        assert!(
            RefKind::parse("keyring:x")
                .unwrap()
                .allowed_in_project_layer()
        );
        assert!(RefKind::parse("env:X").unwrap().allowed_in_project_layer());
        assert!(
            !RefKind::parse(r#"cmd:["a"]"#)
                .unwrap()
                .allowed_in_project_layer()
        );
        assert!(
            !RefKind::parse("file:/a")
                .unwrap()
                .allowed_in_project_layer()
        );
    }
}
