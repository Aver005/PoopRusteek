//! Что делается при запуске, когда конфиг и данные не изолированы флагами.
//!
//! Один раз за процесс: перенос старой папки данных, копия старого конфига,
//! перенос секретов в хранилище. Все шаги безопасно повторяются.

use super::lock::FileLock;
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
    choose_data_dir_within(old, new, MIGRATION_WAIT, MIGRATION_STALE)
}

/// Замок переноса: тот, кто взял его, пульсирует после каждого файла, поэтому «брошенным»
/// он считается только если держатель молчит целую минуту (упал).
const MIGRATION_STALE: std::time::Duration = std::time::Duration::from_secs(60);
const MIGRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(75);

/// Нужен ли перенос: метки нет вовсе, либо есть, но старый бинарь дописывал файлы после неё.
fn needs_move(old: &Path, new: &Path) -> bool {
    match migrate::marker_started(new) {
        None => true,
        Some(since) => migrate::old_has_news(old, since),
    }
}

fn choose_data_dir_within(
    old: Option<PathBuf>,
    new: PathBuf,
    wait: std::time::Duration,
    stale: std::time::Duration,
) -> (PathBuf, Vec<String>) {
    let Some(old) = old else {
        return (new, Vec::new());
    };
    if old == new || !old.is_dir() || !needs_move(&old, &new) {
        return (new, Vec::new());
    }
    // Перенос идёт под замком: второй запуск ждёт, потом видит метку первого и берёт ту же папку.
    let lock = std::fs::create_dir_all(&new)
        .and_then(|()| FileLock::acquire_within(&new.join(".migrate"), wait, stale));
    let guard = match lock {
        Ok(guard) => guard,
        Err(error) => {
            // Не дождались: тот, кто держит замок, ведёт перенос в эту же папку. Остаёмся с ней,
            // чтобы обе копии видели одно; недостающее подберёт следующий запуск.
            return (
                new.clone(),
                vec![format!(
                    "Another copy is still moving the data to {} ({error}); using the new folder. What it has not copied yet appears at the next start.",
                    new.display()
                )],
            );
        }
    };
    // Пока ждали, другой запуск мог закончить.
    if !needs_move(&old, &new) {
        return (new, Vec::new());
    }
    let already_moved_once = migrate::marker_started(&new).is_some();
    let report = migrate::migrate_data_dir_ticking(&old, &new, &|| guard.touch());
    drop(guard);
    if already_moved_once && !report.failed.is_empty() {
        // Дополнение не удалось, но новая папка уже дом: остаёмся в ней, метка прежняя —
        // следующий запуск повторит.
        let notice = format!(
            "Some files from the old folder {} could not be merged into {} ({}); they will be tried again at the next start.",
            old.display(),
            new.display(),
            report.failed.join("; ")
        );
        return (new, vec![notice]);
    }
    if already_moved_once
        && report.failed.is_empty()
        && report.copied == 0
        && report.kept_both.is_empty()
    {
        // Подобрать было нечего (файлы только тронуты): не шумим при каждом запуске.
        return (new, Vec::new());
    }
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

    // ── перенос данных: гонка запусков, падение, старый бинарь ────────────

    use std::time::{Duration, SystemTime};

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Первый запуск ещё переносит; второй ждёт, потом видит метку и берёт ту же папку, ничего не копируя.
    #[test]
    fn the_second_first_start_waits_and_then_uses_the_same_folder() {
        let root = dir("race");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s.json"), "{}");
        std::fs::create_dir_all(&new).unwrap();
        let first = FileLock::acquire(&new.join(".migrate")).unwrap();

        let (o, n) = (old.clone(), new.clone());
        let second = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let chosen =
                choose_data_dir_within(Some(o), n, Duration::from_secs(30), MIGRATION_STALE);
            (chosen, started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(400));
        let report = migrate::migrate_data_dir(&old, &new);
        assert!(report.failed.is_empty(), "{report:?}");
        drop(first);

        let ((chosen, notices), waited) = second.join().unwrap();
        assert_eq!(chosen, new, "nobody stays on the old folder");
        assert!(
            notices.is_empty(),
            "the second start copied nothing itself: {notices:?}"
        );
        assert!(
            waited >= Duration::from_millis(300),
            "it waited: {waited:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Не дождались замка живого держателя: берём новую папку (не старую) и ничего не копируем сами.
    #[test]
    fn a_start_that_cannot_get_the_lock_still_uses_the_new_folder() {
        let root = dir("timeout");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s.json"), "{}");
        std::fs::create_dir_all(&new).unwrap();
        let _holder = FileLock::acquire(&new.join(".migrate")).unwrap();
        let (chosen, notices) = choose_data_dir_within(
            Some(old),
            new.clone(),
            Duration::from_millis(200),
            MIGRATION_STALE,
        );
        assert_eq!(chosen, new);
        assert!(notices[0].contains("Another copy"), "{notices:?}");
        assert!(
            !new.join("sessions/s.json").exists(),
            "it moved nothing itself"
        );
        assert!(!new.join(paths::DATA_MARKER).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Падение посреди переноса: часть файлов лежит, метки нет, замок брошен. Повтор доделывает.
    #[test]
    fn a_crash_in_the_middle_is_repeated_safely() {
        let root = dir("crash");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/a.json"), "a");
        put(&old.join("sessions/b.json"), "b");
        put(&new.join("sessions/a.json"), "a");
        let lock = crate::config::lock::lock_path(&new.join(".migrate"));
        put(&lock, "999999");
        let old_time = SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old_time)
            .unwrap();

        let (chosen, notices) = choose_data_dir(Some(old), new.clone());
        assert_eq!(chosen, new);
        assert!(notices[0].contains("moved"), "{notices:?}");
        assert_eq!(
            std::fs::read_to_string(new.join("sessions/b.json")).unwrap(),
            "b"
        );
        assert!(new.join(paths::DATA_MARKER).exists());
        assert!(!lock.exists(), "the lock is released");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Старый бинарь дописал сессию и поправил файл после переноса: следующий запуск ничего не теряет.
    #[test]
    fn what_a_still_running_old_binary_wrote_is_merged_at_the_next_start() {
        let root = dir("oldbin");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s1.json"), "first");
        put(&old.join("whitelist.json"), "[\"before\"]");
        let (chosen, _) = choose_data_dir(Some(old.clone()), new.clone());
        assert_eq!(chosen, new);

        // Старый бинарь живёт и пишет в свою папку.
        std::thread::sleep(Duration::from_millis(80));
        put(&old.join("sessions/s2.json"), "from the old binary");
        put(&old.join("whitelist.json"), "[\"after\"]");

        let (chosen, notices) = choose_data_dir(Some(old.clone()), new.clone());
        assert_eq!(chosen, new);
        assert!(notices[0].contains("moved"), "{notices:?}");
        assert_eq!(
            std::fs::read_to_string(new.join("sessions/s2.json")).unwrap(),
            "from the old binary",
            "the new session is not lost"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("whitelist.json")).unwrap(),
            "[\"after\"]",
            "the freshest wins"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("whitelist.json.legacy")).unwrap(),
            "[\"before\"]",
            "the other one lies beside it"
        );
        // Дальше тихо: метка обновлена, повтор ничего не делает.
        let (_, notices) = choose_data_dir(Some(old), new);
        assert!(notices.is_empty(), "{notices:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Дополнение не удалось: новая папка остаётся домом, метка прежняя — повторим в следующий раз.
    #[test]
    fn a_failed_top_up_stays_on_the_new_folder_and_is_tried_again() {
        let root = dir("topup");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s1.json"), "first");
        choose_data_dir(Some(old.clone()), new.clone());
        let before = migrate::marker_started(&new);

        std::thread::sleep(Duration::from_millis(80));
        put(&old.join("sessions/s2.json"), "later");
        std::fs::create_dir_all(new.join("sessions/s2.json")).unwrap(); // записи мешает папка
        let (chosen, notices) = choose_data_dir(Some(old.clone()), new.clone());
        assert_eq!(chosen, new, "never back to the old folder");
        assert!(notices[0].contains("tried again"), "{notices:?}");
        assert_eq!(
            migrate::marker_started(&new),
            before,
            "the marker is not refreshed"
        );

        std::fs::remove_dir_all(new.join("sessions/s2.json")).unwrap();
        let (_, notices) = choose_data_dir(Some(old), new.clone());
        assert!(notices[0].contains("moved"), "{notices:?}");
        assert_eq!(
            std::fs::read_to_string(new.join("sessions/s2.json")).unwrap(),
            "later"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
