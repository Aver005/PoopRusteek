//! Что модель не должна читать и писать своими инструментами.
//!
//! Честно: это страхует от случайного чтения файла с ключом через `read_file`. Агент с
//! `bash` работает от имени того же пользователя и дотянется до любого файла и до
//! системного хранилища; шифрование и запреты здесь от него не защищают.

use super::fields;
use super::paths;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Файлы, на которые ссылается `file:` в конфиге: в них лежит секрет.
static SECRET_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub fn register_secret_file(path: PathBuf) {
    if let Ok(mut files) = SECRET_FILES.lock()
        && !files.contains(&path)
    {
        files.push(path);
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    crate::safe_write::resolve_for_compare(a) == crate::safe_write::resolve_for_compare(b)
}

fn inside(path: &Path, dir: &Path) -> bool {
    let (path, dir) = (
        crate::safe_write::resolve_for_compare(path),
        crate::safe_write::resolve_for_compare(dir),
    );
    path.starts_with(dir)
}

/// Папки, куда модель не пишет: данные и устаревшие места.
pub fn protected_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![super::Config::data_dir(), paths::global_dir()];
    dirs.extend(paths::legacy_data_dir());
    dirs.extend(
        paths::legacy_config_path()
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf),
    );
    dirs
}

/// Отдельные файлы, куда модель не пишет: конфиг из `--config` и файлы секретов.
pub fn protected_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = paths::explicit_config()
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    if let Ok(secret_files) = SECRET_FILES.lock() {
        files.extend(secret_files.iter().cloned());
    }
    files
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Любой `config.toml` и его копия `.pre-gateway`: где бы ни лежали (в том числе забытые копии
/// старого конфига внутри папки данных). Читать их модели можно, пока в них нет открытых ключей.
fn is_config_like(path: &Path) -> bool {
    let name = file_name(path);
    name.strip_suffix(".pre-gateway").unwrap_or(&name) == "config.toml"
}

/// Лежит ли файл прямо в папке с таким именем (`.../opencode/auth.json`).
fn has_parent_named(path: &Path, name: &str) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|parent| parent.to_string_lossy().eq_ignore_ascii_case(name))
}

fn mcp_holds_secrets(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return true;
    };
    let non_empty = |v: &serde_json::Value| v.as_object().is_some_and(|o| !o.is_empty());
    value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|servers| {
            servers.values().any(|server| {
                server.get("env").is_some_and(non_empty)
                    || server.get("headers").is_some_and(non_empty)
            })
        })
}

/// Причина отказа читать файл или `None`. Содержимое смотрим только у файлов, где
/// секрет возможен, поэтому обычный конфигурационный файл без ключей читается.
pub fn read_refusal(path: &Path) -> Option<String> {
    let shown = path.display();
    let refuse = |what: &str| {
        Some(format!(
            "Refusing to read {shown}: {what}. Ask the user to manage keys with /secrets instead."
        ))
    };
    if let Ok(files) = SECRET_FILES.lock()
        && files.iter().any(|f| same_path(path, f))
    {
        return refuse("it is a secret file referenced by the config");
    }
    if inside(path, &super::Config::data_dir().join("secrets")) {
        return refuse("it is in the agent's secrets folder");
    }
    // Входы CLI-аккаунтов и токены подписок: утекли бы провайдеру модели вместе с ответом.
    if inside(path, &paths::global_dir().join("accounts")) {
        return refuse("it is in an account's CLI login folder");
    }
    let name = file_name(path);
    if name == ".credentials.json" || (name == "auth.json" && has_parent_named(path, "opencode")) {
        return refuse("it holds a CLI login token");
    }
    let is_mcp = name == "mcp.json"
        && (inside(path, &super::Config::data_dir())
            || paths::legacy_data_dir().is_some_and(|d| inside(path, &d)));
    if !is_config_like(path) && !is_mcp {
        return None;
    }
    // Нет файла или не читается — пусть инструмент сам скажет обычную ошибку.
    let text = crate::util::read_text(path).ok()?;
    let holds_secrets = if is_mcp {
        mcp_holds_secrets(&text)
    } else {
        fields::contains_inline_secrets(&text)
    };
    if holds_secrets {
        refuse(if is_mcp {
            "it holds keys or tokens of MCP servers"
        } else {
            "it holds keys in plain text"
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-protect-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_project_config_with_a_key_is_not_readable_but_a_clean_one_is() {
        let root = dir("proj");
        let layer = root.join(".pooprusteek");
        std::fs::create_dir_all(&layer).unwrap();
        let path = layer.join("config.toml");
        std::fs::write(&path, "[provider]\ntoken = \"canary-token-0001\"\n").unwrap();
        let refusal = read_refusal(&path).expect("must refuse");
        assert!(!refusal.contains("canary"), "{refusal}");
        std::fs::write(
            &path,
            "[provider]\ntoken = \"keyring:global/deepseek-token\"\n",
        )
        .unwrap();
        assert!(read_refusal(&path).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Забытая копия старого конфига в папке данных (Windows/macOS: конфиг лежал там же).
    #[test]
    fn a_forgotten_copy_of_the_old_config_in_any_folder_is_refused() {
        let root = dir("forgotten");
        let folder = root.join("pooprusteek");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        std::fs::write(
            &path,
            "[[providers]]
name = \"a\"
base_url = \"http://x\"
api_key = \"canary-key-000000\"
",
        )
        .unwrap();
        assert!(read_refusal(&path).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_config_backup_with_keys_is_refused() {
        let root = dir("bak");
        let path = root.join("config.toml.pre-gateway");
        std::fs::write(&path, "[provider]\ntoken = \"canary-token-0001\"\n").unwrap();
        assert!(read_refusal(&path).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn registered_secret_files_are_refused() {
        let root = dir("file");
        let path = root.join("key.txt");
        std::fs::write(&path, "k").unwrap();
        assert!(read_refusal(&path).is_none());
        register_secret_file(path.clone());
        assert!(read_refusal(&path).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cli_login_files_are_refused_and_lookalikes_are_not() {
        // Файлы не читаются: отказ выдаётся по пути, их может и не быть.
        let accounts = paths::global_dir().join("accounts").join("work");
        assert!(read_refusal(&accounts.join("anything.txt")).is_some());
        let root = dir("logins");
        for refused in [
            root.join(".claude").join(".credentials.json"),
            root.join("opencode").join("auth.json"),
        ] {
            assert!(read_refusal(&refused).is_some(), "{}", refused.display());
        }
        for fine in [
            root.join("auth.json"),
            root.join("notes").join("credentials.json"),
        ] {
            assert!(read_refusal(&fine).is_none(), "{}", fine.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ordinary_files_are_left_alone() {
        let root = dir("plain");
        let path = root.join("notes.txt");
        std::fs::write(&path, "token = canary").unwrap();
        assert!(read_refusal(&path).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mcp_servers_with_env_values_count_as_secret_holders() {
        assert!(mcp_holds_secrets(
            "{\"mcpServers\":{\"a\":{\"env\":{\"K\":\"v\"}}}}"
        ));
        assert!(!mcp_holds_secrets(
            "{\"mcpServers\":{\"a\":{\"command\":\"x\"}}}"
        ));
        assert!(mcp_holds_secrets("{broken"));
    }
}
