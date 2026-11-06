//! Что делается при запуске, когда конфиг и данные не изолированы флагами.
//!
//! Один раз за процесс: перенос старой папки данных, копия старого конфига,
//! перенос секретов в хранилище. Все шаги безопасно повторяются.

use super::{migrate, paths};
use crate::secrets::{SecretStore, process_store};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

static CONFIG_STARTED: AtomicBool = AtomicBool::new(false);
static DATA_STARTED: AtomicBool = AtomicBool::new(false);

/// Запускать ли шаг один раз за процесс и вне изолированного прогона.
fn should_run(isolated: bool, started: &AtomicBool) -> bool {
    !isolated && !started.swap(true, Ordering::SeqCst)
}

/// Выбор папки данных. Зовётся из `main` до журнала и хранилища откатов.
/// Возвращает пометки для показа пользователю.
pub fn prepare_data_dir() -> Vec<String> {
    if !should_run(paths::is_isolated(), &DATA_STARTED) {
        return Vec::new();
    }
    let (chosen, notices) = choose_data_dir(paths::legacy_data_dir(), paths::default_data_dir());
    paths::set_startup_data_dir(chosen);
    notices
}

/// Какую папку данных взять: новую после переноса или старую, если перенос не удался.
fn choose_data_dir(old: Option<PathBuf>, new: PathBuf) -> (PathBuf, Vec<String>) {
    let Some(old) = old else {
        return (new, Vec::new());
    };
    if old == new || !old.is_dir() || new.join(paths::DATA_MARKER).exists() {
        return (new, Vec::new());
    }
    let report = migrate::migrate_data_dir(&old, &new);
    if report.failed.is_empty() {
        let mut notices = vec![format!(
            "Data moved to {} ({} file(s) copied). The old folder {} was left in place; delete it when you are sure.",
            new.display(),
            report.copied,
            old.display()
        )];
        if !report.kept_both.is_empty() {
            notices.push(format!(
                "{} file(s) existed in both places; the fresher one is in use, the other is kept as *.legacy: {}",
                report.kept_both.len(),
                report.kept_both.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !report.left_behind.is_empty() {
            notices.push(format!(
                "Not copied (left in the old folder, may hold keys): {}",
                report.left_behind.join(", ")
            ));
        }
        (new, notices)
    } else {
        // Не всё доехало: работаем со старой папкой, чтобы ничего не «пропало».
        let notice = format!(
            "Could not move data to {} ({} file(s) failed: {}); still using {}. The move will be retried on the next start.",
            new.display(),
            report.failed.len(),
            report.failed.join("; "),
            old.display()
        );
        (old, vec![notice])
    }
}

/// Перенос конфига и секретов. Один раз за процесс; пометки для пользователя.
pub fn prepare_config(global: &Path, store: &Arc<SecretStore>) -> Vec<String> {
    if !should_run(paths::is_isolated(), &CONFIG_STARTED) {
        return Vec::new();
    }
    prepare_config_with(global, paths::legacy_config_path(), store)
}

fn prepare_config_with(
    global: &Path,
    legacy: Option<PathBuf>,
    store: &Arc<SecretStore>,
) -> Vec<String> {
    let mut notices = Vec::new();
    let legacy = legacy.filter(|legacy| legacy != global);
    if let Some(legacy) = &legacy {
        match migrate::copy_legacy_config(legacy, global) {
            migrate::LegacyConfig::Copied => notices.push(format!(
                "Config copied from {} to {}.",
                legacy.display(),
                global.display()
            )),
            migrate::LegacyConfig::Failed(reason) => {
                notices.push(format!("Could not copy the old config: {reason}."));
            }
            migrate::LegacyConfig::NotNeeded => {}
        }
    }
    match migrate::run(global, store) {
        Ok(report) => notices.extend(report.notices()),
        Err(error) => notices.push(format!("Secrets were not moved to the keyring: {error}")),
    }
    if let Some(legacy) = &legacy
        && migrate::scrub_legacy_config(legacy, global, store)
    {
        notices.push(format!(
            "The old config {} was kept for a rollback, with its keys blanked now that they are in the keyring.",
            legacy.display()
        ));
    }
    notices
}

/// После удачной загрузки, где все ссылки читаются: копия файла больше не хранит ключи.
pub fn after_verified_load(global: &Path) {
    if !paths::is_isolated() {
        migrate::scrub_backup(global);
    }
}

/// Дорабатывает перенос в фоне: после сохранения в файл мог попасть новый открытый ключ.
/// Хранилище и диск не трогаем на цикле событий.
pub fn kick_background_migration(path: &Path) {
    if paths::is_isolated() || cfg!(test) {
        return;
    }
    let store = process_store();
    if store.known_down().is_some() {
        return;
    }
    let path = path.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("secret-migration".to_string())
        .spawn(move || {
            if let Err(error) = migrate::run(&path, &store) {
                tracing::warn!("background secret migration: {error}");
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::fake::{Mode, store};

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-startup-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const OLD: &str = "[provider]\ntoken = \"ds-token-1234567890\"\n";

    /// Изолированный прогон (`--config`/`--data-dir`) ничего не запускает, а остальные — раз.
    #[test]
    fn isolated_runs_never_start_a_step_and_others_start_it_once() {
        let flag = AtomicBool::new(false);
        assert!(!should_run(true, &flag));
        assert!(
            !flag.load(Ordering::SeqCst),
            "an isolated run must not even claim the step"
        );
        assert!(should_run(false, &flag));
        assert!(!should_run(false, &flag), "only once per process");
    }

    #[test]
    fn the_old_config_moves_its_key_to_the_keyring_and_is_retired() {
        let root = dir("cfg");
        let (old, new) = (
            root.join("old").join("config.toml"),
            root.join("new").join("config.toml"),
        );
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, OLD).unwrap();
        let (backend, store) = store();
        let notices = prepare_config_with(&new, Some(old.clone()), &Arc::new(store));
        assert!(notices.iter().any(|n| n.contains("copied")), "{notices:?}");
        assert!(
            !std::fs::read_to_string(&new)
                .unwrap()
                .contains("ds-token-1234567890")
        );
        assert_eq!(
            backend.data.lock().unwrap()["global/deepseek-token"].expose(),
            "ds-token-1234567890"
        );
        assert!(
            old.exists()
                && !std::fs::read_to_string(&old)
                    .unwrap()
                    .contains("ds-token-1234567890"),
            "the old file stays for a rollback, without the key"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn without_a_keyring_both_files_keep_the_key_and_nothing_is_lost() {
        let root = dir("cfgdown");
        let (old, new) = (
            root.join("old").join("config.toml"),
            root.join("new").join("config.toml"),
        );
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, OLD).unwrap();
        let (backend, store) = store();
        backend.mode(Mode::Unavailable);
        let notices = prepare_config_with(&new, Some(old.clone()), &Arc::new(store));
        assert!(
            notices
                .iter()
                .any(|n| n.contains("stays in the config file")),
            "{notices:?}"
        );
        assert!(
            std::fs::read_to_string(&new)
                .unwrap()
                .contains("ds-token-1234567890")
        );
        assert!(
            old.exists(),
            "the old file is the second copy of a key with no safe home yet"
        );
        // Позже, когда хранилище появилось, перенос завершается на новом месте.
        backend.mode(Mode::Healthy);
        let store = Arc::new(SecretStore::new(backend));
        prepare_config_with(&new, Some(old.clone()), &store);
        assert!(
            !std::fs::read_to_string(&new)
                .unwrap()
                .contains("ds-token-1234567890")
        );
        assert!(
            !std::fs::read_to_string(&old)
                .unwrap()
                .contains("ds-token-1234567890"),
            "the old file is blanked, not deleted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_new_config_is_left_alone_and_the_old_one_stays() {
        let root = dir("cfgbroken");
        let (old, new) = (
            root.join("old").join("config.toml"),
            root.join("new").join("config.toml"),
        );
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&old, OLD).unwrap();
        let broken = "[provider\ntoken = \"ds-token-1234567890\"\n";
        std::fs::write(&new, broken).unwrap();
        let (backend, store) = store();
        let notices = prepare_config_with(&new, Some(old.clone()), &Arc::new(store));
        assert!(
            notices.iter().any(|n| n.contains("not moved")),
            "{notices:?}"
        );
        assert_eq!(std::fs::read_to_string(&new).unwrap(), broken);
        assert!(old.exists());
        assert!(backend.data.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn data_comes_from_the_new_folder_after_a_move_and_from_the_old_one_after_a_failure() {
        let root = dir("data");
        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::create_dir_all(old.join("sessions")).unwrap();
        std::fs::write(old.join("sessions").join("s.json"), "{}").unwrap();
        let (chosen, notices) = choose_data_dir(Some(old.clone()), new.clone());
        assert_eq!(chosen, new);
        assert!(notices[0].contains("moved"));
        assert!(new.join("sessions").join("s.json").exists());
        assert!(old.join("sessions").join("s.json").exists());

        let (old2, new2) = (root.join("old2"), root.join("new2"));
        std::fs::create_dir_all(&old2).unwrap();
        std::fs::write(old2.join("history.json"), "[]").unwrap();
        std::fs::create_dir_all(new2.join("history.json")).unwrap(); // записи мешает папка
        let (chosen, notices) = choose_data_dir(Some(old2.clone()), new2);
        assert_eq!(chosen, old2, "a failed move keeps working on the old data");
        assert!(notices[0].contains("retried"), "{notices:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_that_was_already_moved_is_not_moved_again() {
        let root = dir("datadone");
        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("history.json"), "[]").unwrap();
        choose_data_dir(Some(old.clone()), new.clone());
        std::fs::write(old.join("later.json"), "x").unwrap();
        let (chosen, notices) = choose_data_dir(Some(old), new.clone());
        assert_eq!(chosen, new);
        assert!(notices.is_empty());
        assert!(
            !new.join("later.json").exists(),
            "the marker makes the move a one-off"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
