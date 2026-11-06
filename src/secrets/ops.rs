//! Действия человека над секретами: список, задать, удалить, проверить.
//! Общие для `pooprusteek secrets` и `/secrets`. Значения не возвращаются и не печатаются.

use super::{RefKind, Secret, SecretError, SecretStore, account_secret_id, keyring_ref};
use crate::config;

/// Одна строка списка: где лежит ссылка, куда ведёт, что с ней.
#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub reference: String,
    pub state: String,
}

impl Row {
    pub fn line(&self) -> String {
        format!("{:<34} {:<38} {}", self.label, self.reference, self.state)
    }
}

fn keyring_state(store: &SecretStore, id: &str) -> String {
    match store.get(id) {
        Ok(Some(secret)) => format!("ok (…{})", secret.fingerprint()),
        Ok(None) => "missing: not set in the keyring".to_string(),
        Err(error) if error.is_unavailable() => format!("keyring unavailable: {error}"),
        Err(error) => error.to_string(),
    }
}

/// Список ссылок из файла конфига и открытых секретов, что там ещё остались.
/// Команды `cmd:` не запускаются: проверка живьём — в `doctor`.
pub fn list(store: &SecretStore) -> Vec<Row> {
    let mut rows = Vec::new();
    for found in config::file_references() {
        let state = match RefKind::parse(&found.reference) {
            Ok(RefKind::Keyring(id)) => keyring_state(store, &id),
            Ok(RefKind::Env(name)) => {
                if std::env::var(&name).is_ok_and(|v| !v.trim().is_empty()) {
                    "set (env: less protected, children inherit it)".to_string()
                } else {
                    "missing: variable is not set".to_string()
                }
            }
            Ok(RefKind::File(path)) => {
                if path.is_file() {
                    "file found".to_string()
                } else {
                    "missing: file not found".to_string()
                }
            }
            Ok(RefKind::Command(_)) => "command (not run here)".to_string(),
            Ok(RefKind::None) => "no secret needed".to_string(),
            Err(reason) => format!("invalid: {reason}"),
        };
        rows.push(Row {
            label: found.label,
            reference: found.reference,
            state,
        });
    }
    for label in config::inline_secret_labels() {
        rows.push(Row {
            label,
            reference: "(plain text in the file)".to_string(),
            state: "will be moved to the keyring when it is available".to_string(),
        });
    }
    rows
}

/// Имя записи: `global/my-key` как есть, голое имя аккаунта — по общему правилу.
pub fn entry_id(name: &str) -> Result<String, String> {
    let id = if name.contains('/') {
        name.to_string()
    } else {
        account_secret_id(name)
    };
    RefKind::parse(&keyring_ref(&id))?;
    Ok(id)
}

/// Записывает секрет и проверяет чтением. Связывает с аккаунтом или провайдером того же имени.
pub fn set(store: &SecretStore, name: &str, value: Secret) -> Result<String, String> {
    if value.is_empty() {
        return Err("the value is empty".to_string());
    }
    let id = entry_id(name)?;
    store
        .set_verified(&id, &value)
        .map_err(|e: SecretError| e.to_string())?;
    super::redact::register(&value);
    let reference = keyring_ref(&id);
    let mut message = format!("Stored {reference} (…{}).", value.fingerprint());
    if !name.contains('/') {
        match config::link_secret_reference(name, &reference) {
            Ok(Some(label)) => message.push_str(&format!(" Linked to {label}.")),
            Ok(None) => message.push_str(&format!(
                " Use it in the config as secret = \"{reference}\"."
            )),
            Err(error) => message.push_str(&format!(" Could not update the config: {error}")),
        }
    } else {
        message.push_str(&format!(
            " Use it in the config as secret = \"{reference}\"."
        ));
    }
    Ok(message)
}

/// Удаляет запись из хранилища. Ссылки в конфиге остаются, `doctor` их покажет.
pub fn remove(store: &SecretStore, name: &str) -> Result<String, String> {
    let id = entry_id(name)?;
    store.delete(&id).map_err(|e| e.to_string())?;
    let still_used = config::file_references()
        .iter()
        .any(|r| r.reference == keyring_ref(&id));
    Ok(if still_used {
        format!(
            "Removed {id}. The config still refers to it; point it elsewhere or run `pooprusteek doctor`."
        )
    } else {
        format!("Removed {id}.")
    })
}

/// Круг записать–прочитать–удалить, в том числе значения длиннее предела Windows.
pub fn selftest(store: &SecretStore) -> Vec<String> {
    // Проверка повторяется после починки хранилища: прежний отказ не помним.
    store.forget_down();
    let mut out = Vec::new();
    match store.probe() {
        Ok(()) => out.push(format!(
            "{}: short value round trip ok",
            store.backend_name()
        )),
        Err(error) => {
            out.push(format!("{}: FAILED: {error}", store.backend_name()));
            return out;
        }
    }
    let id = format!("probe/long-{}", std::process::id());
    let long = Secret::new("x".repeat(5_000));
    match store.set_verified(&id, &long) {
        Ok(()) => out.push("long value (5000 bytes, split into parts) round trip ok".to_string()),
        Err(error) => out.push(format!("long value FAILED: {error}")),
    }
    let _ = store.delete(&id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::fake::store;

    #[test]
    fn names_map_to_entries_and_junk_is_refused() {
        assert_eq!(entry_id("openai").unwrap(), "global/openai");
        assert_eq!(entry_id("global/team-key").unwrap(), "global/team-key");
        assert!(entry_id("bad/../x").is_err());
        assert!(entry_id("a b/c").is_err());
    }

    #[test]
    fn set_stores_a_verified_value_and_never_echoes_it() {
        let (backend, store) = store();
        let message = set(
            &store,
            "global/canary-entry",
            Secret::new("canary-op-value-1"),
        )
        .unwrap();
        assert!(!message.contains("canary-op-value"), "{message}");
        assert_eq!(
            backend.data.lock().unwrap()["global/canary-entry"].expose(),
            "canary-op-value-1"
        );
    }

    #[test]
    fn an_empty_value_is_refused() {
        let (_, store) = store();
        assert!(set(&store, "global/x", Secret::new("")).is_err());
    }

    #[test]
    fn remove_deletes_and_selftest_reports_both_round_trips() {
        let (backend, store) = store();
        set(&store, "global/y", Secret::new("value-yyyyyyyy")).unwrap();
        remove(&store, "global/y").unwrap();
        assert!(backend.data.lock().unwrap().is_empty());
        let report = selftest(&store);
        assert_eq!(report.len(), 2, "{report:?}");
        assert!(report.iter().all(|l| l.contains("ok")), "{report:?}");
        assert!(
            backend.data.lock().unwrap().is_empty(),
            "selftest cleans up"
        );
    }
}
