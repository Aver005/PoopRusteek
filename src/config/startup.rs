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

/// Дополнение (метка уже есть): дешёвая проверка времени каталога `sessions` старой папки на
/// главном потоке, сам обход — в фоне. Папка данных от этого не зависит, поэтому ничего не ждём.
fn top_up_in_background(old: &Path, new: &Path, since: std::time::SystemTime) {
    if !migrate::sessions_dir_changed(old, since) {
        return;
    }
    let (old, new) = (old.to_path_buf(), new.to_path_buf());
    let _ = std::thread::Builder::new()
        .name("data-top-up".to_string())
        .spawn(move || top_up(&old, &new));
}

/// Дополнение под тем же замком, что и перенос. Занят замок — пропускаем, подберёт следующий запуск.
fn top_up(old: &Path, new: &Path) {
    let Ok(guard) = FileLock::acquire_within(
        &new.join(".migrate"),
        MIGRATION_TOP_UP_WAIT,
        MIGRATION_STALE,
    ) else {
        return;
    };
    if let Err(error) = migrate::top_up_sessions(old, new, &|| guard.touch()) {
        tracing::warn!("sessions of the old folder were not merged: {error}");
    }
}

const MIGRATION_TOP_UP_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

fn choose_data_dir_within(
    old: Option<PathBuf>,
    new: PathBuf,
    wait: std::time::Duration,
    stale: std::time::Duration,
) -> (PathBuf, Vec<String>) {
    let Some(old) = old else {
        return (new, Vec::new());
    };
    if old == new || !old.is_dir() {
        return (new, Vec::new());
    }
    if let Some(since) = migrate::marker_started(&new) {
        top_up_in_background(&old, &new, since);
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
    if migrate::marker_started(&new).is_some() {
        return (new, Vec::new());
    }
    let report = migrate::migrate_data_dir_ticking(&old, &new, &|| guard.touch());
    drop(guard);
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

    /// Ждём, пока фоновое дополнение запишет в метку нужную сессию.
    fn wait_for_session_in_marker(new: &Path, name: &str) {
        for _ in 0..200 {
            let text = std::fs::read_to_string(new.join(paths::DATA_MARKER)).unwrap_or_default();
            if text.contains(name) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the top-up never recorded {name}");
    }

    /// Старый бинарь завёл сессию после переноса — она приходит; удалённая человеком не возвращается,
    /// настройки старой папки не дополняются, существующее в новой папке не затирается.
    #[test]
    fn a_new_session_of_the_old_binary_comes_but_a_deleted_one_never_returns() {
        let root = dir("oldbin");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s1.json"), "first");
        put(&old.join("sessions/s2.json"), "second");
        put(&old.join("whitelist.json"), "[\"before\"]");
        put(&old.join("mcp.json"), "{\"mcpServers\":{}}");
        let (chosen, _) = choose_data_dir(Some(old.clone()), new.clone());
        assert_eq!(chosen, new);
        std::fs::write(
            new.join("sessions/s2.json"),
            "second, continued in the new app",
        )
        .unwrap();

        // Человек удалил s1 в новой папке; живой старый бинарь пишет дальше.
        std::fs::remove_file(new.join("sessions/s1.json")).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        put(&old.join("sessions/s3.json"), "from the old binary");
        put(
            &old.join("sessions/s2.json"),
            "second, touched by the old binary",
        );
        put(
            &old.join("sessions/s1.json"),
            "first, touched by the old binary",
        );
        put(&old.join("whitelist.json"), "[\"after\"]");
        put(&old.join("mcp.json"), "{\"mcpServers\":{\"x\":{}}}");
        put(&old.join("skills/new/SKILL.md"), "skill");

        let (chosen, notices) = choose_data_dir(Some(old.clone()), new.clone());
        assert_eq!(chosen, new);
        assert!(
            notices.is_empty(),
            "no scary note for a top-up: {notices:?}"
        );
        wait_for_session_in_marker(&new, "sessions/s3.json");

        assert_eq!(
            std::fs::read_to_string(new.join("sessions/s3.json")).unwrap(),
            "from the old binary",
            "the new session arrived"
        );
        assert!(
            !new.join("sessions/s1.json").exists(),
            "a deleted session stays deleted"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("sessions/s2.json")).unwrap(),
            "second, continued in the new app",
            "what exists in the new folder is not overwritten"
        );
        assert!(!new.join("sessions/s2.json.legacy").exists());
        assert_eq!(
            std::fs::read_to_string(new.join("whitelist.json")).unwrap(),
            "[\"before\"]",
            "settings are not topped up"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("mcp.json")).unwrap(),
            "{\"mcpServers\":{}}"
        );
        assert!(!new.join("skills/new").exists());

        // Дальше тихо: метка знает s3, повторный запуск ничего не меняет.
        let before = std::fs::read_to_string(new.join(paths::DATA_MARKER)).unwrap();
        let (_, notices) = choose_data_dir(Some(old), new.clone());
        assert!(notices.is_empty());
        assert_eq!(
            std::fs::read_to_string(new.join(paths::DATA_MARKER)).unwrap(),
            before,
            "nothing to look at: the sessions folder did not change"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Сначала дешёвая проверка: обход старой папки идёт, только если каталог сессий изменился после метки.
    #[test]
    fn the_old_folder_is_walked_only_when_its_sessions_directory_changed() {
        let root = dir("cheap");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s1.json"), "first");
        choose_data_dir(Some(old.clone()), new.clone());
        let since = migrate::marker_started(&new).unwrap();
        assert!(!migrate::sessions_dir_changed(&old, since));
        std::thread::sleep(Duration::from_millis(80));
        put(&old.join("sessions/s2.json"), "second");
        assert!(migrate::sessions_dir_changed(&old, since));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Метка без списка сессий (не знаем, что человек удалил): дополнять нельзя.
    #[test]
    fn a_marker_without_the_session_list_tops_up_nothing() {
        let root = dir("nolist");
        let (old, new) = (root.join("old"), root.join("new"));
        put(&old.join("sessions/s1.json"), "first");
        choose_data_dir(Some(old.clone()), new.clone());
        let marker = new.join(paths::DATA_MARKER);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("sessions");
        std::fs::write(&marker, value.to_string()).unwrap();
        std::fs::remove_file(new.join("sessions/s1.json")).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        put(&old.join("sessions/s2.json"), "second");
        assert_eq!(migrate::top_up_sessions(&old, &new, &|| {}), Ok(0));
        assert!(!new.join("sessions/s2.json").exists());
        assert!(!new.join("sessions/s1.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
