//! Переносы данных пользователя. Правило одно: ничего не теряется.
//!
//! - секреты из открытого текста конфига → системное хранилище: записали, прочитали
//!   обратно, сравнили, и только потом заменили значение ссылкой;
//! - `[[providers]]` → `[[accounts]]` один раз, с копией файла;
//! - старый конфиг и старая папка данных → новые места, без удаления данных.
//!
//! Любой сбой оставляет файл как был: ключ остаётся на месте, перенос повторится.

use super::fields::{self, Slot};
use super::lock::FileLock;
use super::write::{self, Op, Seg, WriteError};
use crate::gateway::types::{GatewayConfig, SecretRef};
use crate::secrets::{Secret, SecretStore, keyring_ref};
use std::path::{Path, PathBuf};

/// Имя копии файла перед первой правкой миграции.
pub const BACKUP_SUFFIX: &str = ".pre-gateway";

pub fn backup_path(config: &Path) -> PathBuf {
    let mut name = config
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(BACKUP_SUFFIX);
    config.with_file_name(name)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Поля, чьи секреты теперь в хранилище.
    pub moved: Vec<String>,
    /// Поля, оставленные открытым текстом, и почему.
    pub kept: Vec<(String, String)>,
    pub gateway_added: bool,
    pub backup_created: bool,
    pub wrote: bool,
}

impl Report {
    /// Короткие строки для показа пользователю. Значений тут нет.
    pub fn notices(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.moved.is_empty() {
            out.push(format!(
                "Moved {} secret(s) to the system keyring ({}). A backup of the old file is kept until the next verified start.",
                self.moved.len(),
                self.moved.join(", ")
            ));
        }
        for (label, reason) in &self.kept {
            out.push(format!(
                "{label} stays in the config file: {reason}. It will be moved on a later start."
            ));
        }
        if self.gateway_added {
            out.push(
                "Providers were mirrored into [[accounts]] (the old [[providers]] still work)."
                    .to_string(),
            );
        }
        out
    }
}

#[derive(Debug)]
pub enum MigrateError {
    /// Файл повреждён или не читается как конфиг: не трогаем.
    Damaged(String),
    Io(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Damaged(m) | Self::Io(m) => f.write_str(m),
        }
    }
}

impl From<WriteError> for MigrateError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::Damaged(m) => Self::Damaged(m),
            WriteError::Io(m) => Self::Io(m),
        }
    }
}

/// Добавляет `[gateway]` и `[[accounts]]` из старых записей, если шлюз ещё не заводили.
fn add_gateway(doc: &mut toml_edit::DocumentMut, typed: &super::Config) -> bool {
    if doc.contains_key("gateway") {
        return false;
    }
    let (legacy, _problems) = GatewayConfig::from_legacy(typed);
    if legacy.accounts.is_empty() {
        return false;
    }
    let mut ops = vec![Op::Set(
        vec![Seg::Key("gateway".into())],
        toml::Value::Table(toml::Table::from_iter([
            ("schema".to_string(), toml::Value::Integer(1)),
            ("enabled".to_string(), toml::Value::Boolean(false)),
        ])),
    )];
    for mut account in legacy.accounts {
        // Ключ, уже ставший ссылкой, едет как `secret`, а не как `api_key`.
        if let Some(key) = account.api_key.clone()
            && fields::is_reference(&key)
            && let Ok(reference) = SecretRef::parse(&key)
        {
            account.api_key = None;
            account.secret = Some(reference);
        }
        if let Ok(value) = toml::Value::try_from(&account) {
            ops.push(Op::Push(vec![Seg::Key("accounts".into())], value));
        }
    }
    write::apply(doc, &ops).is_empty()
}

/// Перенос секретов и (один раз) аккаунтов. Файл правится одной атомарной записью в конце.
pub fn run(path: &Path, store: &SecretStore) -> Result<Report, MigrateError> {
    let _guard = FileLock::acquire(path)
        .map_err(|e| MigrateError::Io(format!("{}: {e}", path.display())))?;
    let text = match super::lock::retry_transient(|| crate::util::read_text(path)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Report::default()),
        Err(e) => {
            return Err(MigrateError::Io(format!(
                "{}: {}",
                path.display(),
                e.kind()
            )));
        }
    };
    let mut doc = write::parse_document(path, &text)?;
    let typed = super::parse(&text)
        .map_err(|e| MigrateError::Damaged(super::describe::describe(path, &text, &e)))?;
    let mut report = Report {
        gateway_added: add_gateway(&mut doc, &typed),
        ..Report::default()
    };

    // Открытые секреты, сгруппированные по записи хранилища.
    let mut groups: Vec<(String, Secret, Vec<Slot>)> = Vec::new();
    let mut conflicts: Vec<(String, String)> = Vec::new();
    for (slot, value) in fields::plaintext_slots(&mut doc) {
        let id = slot.secret_id();
        match groups.iter_mut().find(|(known, _, _)| *known == id) {
            Some((_, first, slots)) if first.matches(&value) => slots.push(slot),
            Some(_) => conflicts.push((
                slot.label(),
                "a different key is already waiting for the same keyring entry".to_string(),
            )),
            None => groups.push((id, value, vec![slot])),
        }
    }
    report.kept.extend(conflicts);

    if !groups.is_empty() {
        match store.probe() {
            Err(error) => {
                for (_, _, slots) in &groups {
                    for slot in slots {
                        report.kept.push((slot.label(), error.to_string()));
                    }
                }
            }
            Ok(()) => {
                for (id, value, slots) in &groups {
                    match store.set_verified(id, value) {
                        Ok(()) => {
                            let reference = keyring_ref(id);
                            for slot in slots {
                                fields::replace_with_reference(&mut doc, slot, &reference);
                                report.moved.push(slot.label());
                            }
                        }
                        Err(error) => {
                            for slot in slots {
                                report.kept.push((slot.label(), error.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }

    let new_text = doc.to_string();
    if new_text == text {
        return Ok(report);
    }
    // Результат обязан остаться читаемым конфигом, иначе ничего не пишем.
    super::parse(&new_text)
        .map_err(|e| MigrateError::Damaged(super::describe::describe(path, &new_text, &e)))?;
    // Копия до первой правки. Не вышла копия — не правим.
    let backup = backup_path(path);
    if !backup.is_file() {
        write::write_file(&backup, &text).map_err(MigrateError::from)?;
        report.backup_created = true;
    }
    write::write_file(path, &new_text).map_err(MigrateError::from)?;
    report.wrote = true;
    Ok(report)
}

/// После запуска, где все ссылки читаются, копия больше не хранит значения секретов.
pub fn scrub_backup(config: &Path) -> bool {
    let backup = backup_path(config);
    let Ok(text) = crate::util::read_text(&backup) else {
        return false;
    };
    match fields::scrub_plaintext(&text) {
        Some(clean) => write::write_file(&backup, &clean).is_ok(),
        None => false,
    }
}

// ── старый конфиг ───────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum LegacyConfig {
    /// Нового файла достаточно, старого нет, либо новый уже есть.
    NotNeeded,
    Copied,
    Failed(String),
}

/// Копия старого конфига на новое место. Старый файл остаётся.
pub fn copy_legacy_config(legacy: &Path, new: &Path) -> LegacyConfig {
    if new.exists() || !legacy.is_file() {
        return LegacyConfig::NotNeeded;
    }
    let Ok(_guard) = FileLock::acquire(new) else {
        return LegacyConfig::Failed("the new config location is locked".to_string());
    };
    if new.exists() {
        return LegacyConfig::NotNeeded;
    }
    let bytes = match std::fs::read(legacy) {
        Ok(bytes) => bytes,
        Err(e) => return LegacyConfig::Failed(format!("cannot read the old config: {}", e.kind())),
    };
    if let Err(e) = crate::util::atomic_write(new, &bytes) {
        return LegacyConfig::Failed(format!("cannot write the new config: {}", e.kind()));
    }
    write::restrict_to_owner(new);
    match std::fs::read(new) {
        Ok(back) if back == bytes => LegacyConfig::Copied,
        _ => {
            let _ = std::fs::remove_file(new);
            LegacyConfig::Failed("the copy did not read back identically".to_string())
        }
    }
}

/// Старый конфиг с открытыми секретами убираем, когда в новом их уже нет.
/// Возвращает `true`, если файл удалён.
pub fn retire_legacy_config(legacy: &Path, new: &Path) -> bool {
    let (Ok(old_text), Ok(new_text)) =
        (crate::util::read_text(legacy), crate::util::read_text(new))
    else {
        return false;
    };
    let had_secrets = {
        let Ok(mut doc) = old_text.parse::<toml_edit::DocumentMut>() else {
            return false;
        };
        !fields::plaintext_slots(&mut doc).is_empty()
    };
    if !had_secrets
        || fields::contains_inline_secrets(&new_text)
        || super::parse(&new_text).is_err()
    {
        return false;
    }
    std::fs::remove_file(legacy).is_ok()
}

// ── старая папка данных ─────────────────────────────────────────────────

/// Что можно перекачать само и потому не переносится.
const REBUILDABLE_TOP_LEVEL: [&str; 2] = ["models", "semantic"];

#[derive(Debug, Default)]
pub struct DataReport {
    pub copied: usize,
    pub already_there: usize,
    /// Файл есть в обоих местах и различается: старая версия сохранена рядом.
    pub kept_both: Vec<String>,
    pub failed: Vec<String>,
}

fn skip_name(name: &str) -> bool {
    name.ends_with(".log") || name.ends_with(".tmp") || name.ends_with(".lock")
}

fn digest(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let top = relative
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        if REBUILDABLE_TOP_LEVEL.contains(&top.as_str()) {
            continue;
        }
        if path.is_dir() {
            walk_files(root, &path, out);
        } else if !skip_name(&entry.file_name().to_string_lossy()) {
            out.push(relative);
        }
    }
}

/// Копирует данные из старой папки в новую. Старая остаётся нетронутой.
/// Метка в новой папке ставится только после полного успеха: иначе перенос повторится.
pub fn migrate_data_dir(old: &Path, new: &Path) -> DataReport {
    let mut report = DataReport::default();
    let mut files = Vec::new();
    walk_files(old, old, &mut files);
    for relative in files {
        let source = old.join(&relative);
        let target = new.join(&relative);
        let shown = relative.display().to_string();
        let bytes = match std::fs::read(&source) {
            Ok(bytes) => bytes,
            Err(e) => {
                report.failed.push(format!("{shown}: {}", e.kind()));
                continue;
            }
        };
        match std::fs::read(&target) {
            Ok(existing) if existing == bytes => {
                report.already_there += 1;
                continue;
            }
            Ok(_) => {
                // Не затираем ни ту, ни другую версию.
                let mut name = target
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_default();
                name.push(".legacy");
                let side = target.with_file_name(name);
                if crate::util::atomic_write(&side, &bytes).is_ok() {
                    report.kept_both.push(shown);
                } else {
                    report
                        .failed
                        .push(format!("{shown}: cannot keep the old version"));
                }
                continue;
            }
            Err(_) => {}
        }
        if let Err(e) = crate::util::atomic_write(&target, &bytes) {
            report.failed.push(format!("{shown}: {}", e.kind()));
            continue;
        }
        match std::fs::read(&target) {
            Ok(back) if digest(&back) == digest(&bytes) => report.copied += 1,
            _ => {
                let _ = std::fs::remove_file(&target);
                report
                    .failed
                    .push(format!("{shown}: copy did not read back identically"));
            }
        }
    }
    if report.failed.is_empty() {
        let marker = serde_json::json!({
            "from": old.display().to_string(),
            "copied": report.copied,
            "already_there": report.already_there,
            "kept_both": report.kept_both,
        });
        let text = serde_json::to_string_pretty(&marker).unwrap_or_default();
        if let Err(e) =
            crate::util::atomic_write(&new.join(super::paths::DATA_MARKER), text.as_bytes())
        {
            report.failed.push(format!("marker: {}", e.kind()));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::fake::{Mode, store};

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-migrate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LEGACY: &str = r#"# мой конфиг
[provider]
token = "ds-token-1234567890"   # DeepSeek
model = "deepseek-chat"

[server]
api_key = "gate-key-abcdefghij"

[[providers]]
name = "lmstudio"
base_url = "http://localhost:1234/v1"
api_key = "lm-key-0987654321"
model = "qwen"

[[providers]]
name = "plain"
base_url = "http://localhost:1/v1"
"#;

    fn setup(tag: &str, text: &str) -> (PathBuf, PathBuf) {
        let dir = dir(tag);
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn no_plaintext(text: &str) -> bool {
        !text.contains("ds-token-1234567890")
            && !text.contains("gate-key-abcdefghij")
            && !text.contains("lm-key-0987654321")
    }

    #[test]
    fn a_healthy_store_takes_every_secret_and_the_file_keeps_references() {
        let (dir, path) = setup("ok", LEGACY);
        let (backend, store) = store();
        let report = run(&path, &store).unwrap();
        assert!(report.wrote && report.kept.is_empty(), "{report:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(no_plaintext(&text), "{text}");
        assert!(
            text.contains("# мой конфиг") && text.contains("# DeepSeek"),
            "{text}"
        );
        assert!(text.contains("keyring:global/deepseek-token"), "{text}");
        let data = backend.data.lock().unwrap();
        assert_eq!(
            data["global/deepseek-token"].expose(),
            "ds-token-1234567890"
        );
        assert_eq!(
            data["global/server-api-key"].expose(),
            "gate-key-abcdefghij"
        );
        assert_eq!(data["global/lmstudio"].expose(), "lm-key-0987654321");
        // Старые записи [[providers]] остаются читаемыми.
        assert!(text.contains("[[providers]]"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn providers_are_mirrored_into_accounts_once_and_share_the_same_keyring_entry() {
        let (dir, path) = setup("mirror", LEGACY);
        let (_, store) = store();
        let report = run(&path, &store).unwrap();
        assert!(report.gateway_added);
        let config = super::super::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.gateway.schema, 1);
        assert!(!config.gateway.enabled);
        let ids: Vec<&str> = config.accounts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["deepseek", "lmstudio", "plain"]);
        let lm = &config.accounts[1];
        assert_eq!(
            lm.secret.as_ref().unwrap().as_str(),
            "keyring:global/lmstudio"
        );
        assert!(lm.api_key.is_none(), "the key itself must not be mirrored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_with_the_original_text_is_made_once_before_the_first_edit() {
        let (dir, path) = setup("backup", LEGACY);
        let (_, store) = store();
        let report = run(&path, &store).unwrap();
        assert!(report.backup_created);
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), LEGACY);
        // Повторный прогон копию не трогает.
        std::fs::write(&path, format!("{LEGACY}\n# позже\n")).unwrap();
        let _ = run(&path, &store).unwrap();
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), LEGACY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unavailable_store_keeps_every_secret_in_place_and_says_why() {
        let (dir, path) = setup("down", LEGACY);
        let (backend, store) = store();
        backend.mode(Mode::Unavailable);
        let report = run(&path, &store).unwrap();
        assert!(report.moved.is_empty());
        assert_eq!(report.kept.len(), 4, "{report:?}"); // token, server, provider, mirrored account
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("ds-token-1234567890") && text.contains("lm-key-0987654321"),
            "{text}"
        );
        for (_, reason) in &report.kept {
            assert!(!reason.contains("ds-token"), "{reason}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_later_start_with_a_working_store_finishes_the_job() {
        let (dir, path) = setup("retry", LEGACY);
        let (backend, store) = store();
        backend.mode(Mode::Unavailable);
        run(&path, &store).unwrap();
        backend.mode(Mode::Healthy);
        store.forget_down();
        let report = run(&path, &store).unwrap();
        assert_eq!(report.moved.len(), 4, "{report:?}"); // token, server, providers + mirrored account
        assert!(no_plaintext(&std::fs::read_to_string(&path).unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let (dir, path) = setup("failwrite", LEGACY);
        let (backend, store) = store();
        backend.mode(Mode::FailWrite);
        let report = run(&path, &store).unwrap();
        assert!(report.moved.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ds-token-1234567890"), "{text}");
        assert!(backend.data.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_back_failure_keeps_the_plaintext() {
        let (dir, path) = setup("failread", LEGACY);
        let (backend, store) = store();
        // Проба проходит на здоровом хранилище, а настоящее чтение потом ломается.
        backend.mode(Mode::FailRead);
        let report = run(&path, &store).unwrap();
        assert!(report.moved.is_empty(), "{report:?}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("ds-token-1234567890")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_value_that_reads_back_differently_is_never_trusted() {
        let (dir, path) = setup("differs", LEGACY);
        let (backend, store) = store();
        backend.mode(Mode::Differs);
        let report = run(&path, &store).unwrap();
        assert!(report.moved.is_empty(), "{report:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("ds-token-1234567890") && text.contains("gate-key-abcdefghij"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_is_not_touched() {
        let broken = "[provider\ntoken = \"ds-token-1234567890\"\n";
        let (dir, path) = setup("damaged", broken);
        let (backend, store) = store();
        let error = run(&path, &store).unwrap_err();
        assert!(matches!(error, MigrateError::Damaged(_)), "{error}");
        assert!(!error.to_string().contains("ds-token"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert!(backend.data.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_does_not_read_as_a_config_is_not_touched() {
        let typo = "[agent]\nmax_retries = \"x\"\n[provider]\ntoken = \"ds-token-1234567890\"\n";
        let (dir, path) = setup("typed", typo);
        let (_, store) = store();
        assert!(run(&path, &store).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), typo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let (dir, path) = setup("idem", LEGACY);
        let (_, store) = store();
        run(&path, &store).unwrap();
        let after_first = std::fs::read_to_string(&path).unwrap();
        let report = run(&path, &store).unwrap();
        assert_eq!(report, Report::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_that_already_holds_references_is_not_rewritten() {
        let text = "[provider]\ntoken = \"keyring:global/deepseek-token\"\n[gateway]\nschema = 1\n";
        let (dir, path) = setup("refs", text);
        let (backend, store) = store();
        let report = run(&path, &store).unwrap();
        assert_eq!(report, Report::default());
        assert!(
            backend.data.lock().unwrap().is_empty(),
            "no needless writes"
        );
        assert!(!backup_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_backup_aborts_before_the_file_is_changed() {
        let (dir, path) = setup("nobackup", LEGACY);
        std::fs::create_dir_all(backup_path(&path)).unwrap(); // на месте копии папка
        let (_, store) = store();
        assert!(run(&path, &store).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), LEGACY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_different_keys_for_one_entry_are_both_kept() {
        let text = "[[providers]]\nname = \"a\"\nbase_url = \"http://x/v1\"\napi_key = \"first-key-00000000\"\n\n[[accounts]]\nid = \"a\"\nprovider = \"openai\"\napi_key = \"other-key-11111111\"\n\n[gateway]\nschema = 1\n";
        let (dir, path) = setup("conflict", text);
        let (backend, store) = store();
        let report = run(&path, &store).unwrap();
        assert_eq!(report.kept.len(), 1, "{report:?}");
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("other-key-11111111"),
            "the unmatched key must not be lost: {out}"
        );
        assert_eq!(
            backend.data.lock().unwrap()["global/a"].expose(),
            "first-key-00000000"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_is_scrubbed_after_a_verified_start() {
        let (dir, path) = setup("scrub", LEGACY);
        let (_, store) = store();
        run(&path, &store).unwrap();
        assert!(scrub_backup(&path));
        let backup = std::fs::read_to_string(backup_path(&path)).unwrap();
        assert!(no_plaintext(&backup), "{backup}");
        assert!(backup.contains("# мой конфиг"));
        assert!(!scrub_backup(&path), "nothing left to scrub");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── старый конфиг ──

    #[test]
    fn the_old_config_is_copied_once_and_left_in_place() {
        let dir = dir("legacy");
        let old = dir.join("old").join("config.toml");
        let new = dir.join("new").join("config.toml");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, LEGACY).unwrap();
        assert_eq!(copy_legacy_config(&old, &new), LegacyConfig::Copied);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), LEGACY);
        assert!(old.exists());
        std::fs::write(&new, "# новый\n").unwrap();
        assert_eq!(copy_legacy_config(&old, &new), LegacyConfig::NotNeeded);
        assert_eq!(
            std::fs::read_to_string(&new).unwrap(),
            "# новый\n",
            "an existing file wins"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_config_is_retired_only_when_its_secrets_are_safe_elsewhere() {
        let dir = dir("retire");
        let old = dir.join("old.toml");
        let new = dir.join("new.toml");
        std::fs::write(&old, LEGACY).unwrap();
        std::fs::write(&new, LEGACY).unwrap();
        assert!(
            !retire_legacy_config(&old, &new),
            "new still holds plaintext"
        );
        let (_, store) = store();
        run(&new, &store).unwrap();
        assert!(retire_legacy_config(&old, &new));
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_secretless_old_config_is_never_deleted() {
        let dir = dir("keepold");
        let old = dir.join("old.toml");
        let new = dir.join("new.toml");
        std::fs::write(&old, "[ui]\ntheme = \"x\"\n").unwrap();
        std::fs::write(&new, "[ui]\ntheme = \"x\"\n").unwrap();
        assert!(!retire_legacy_config(&old, &new));
        assert!(old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── старая папка данных ──

    fn populate(old: &Path) {
        std::fs::create_dir_all(old.join("sessions")).unwrap();
        std::fs::write(old.join("sessions").join("s1.json"), "{\"id\":1}").unwrap();
        std::fs::write(old.join("history.json"), "[\"hi\"]").unwrap();
        std::fs::write(old.join("whitelist.json"), "[]").unwrap();
        std::fs::write(old.join("checkpoints.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(old.join("models")).unwrap();
        std::fs::write(old.join("models").join("big.onnx"), "x").unwrap();
        std::fs::create_dir_all(old.join("semantic")).unwrap();
        std::fs::write(old.join("semantic").join("history.json"), "x").unwrap();
        std::fs::write(old.join("debug.log"), "x").unwrap();
    }

    #[test]
    fn data_moves_without_loss_and_what_can_be_rebuilt_stays_behind() {
        let dir = dir("data");
        let (old, new) = (dir.join("old"), dir.join("new"));
        populate(&old);
        let report = migrate_data_dir(&old, &new);
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(report.copied, 4);
        assert_eq!(
            std::fs::read_to_string(new.join("sessions/s1.json")).unwrap(),
            "{\"id\":1}"
        );
        assert!(!new.join("models").exists() && !new.join("semantic").exists());
        assert!(!new.join("debug.log").exists());
        assert!(new.join(super::super::paths::DATA_MARKER).exists());
        assert!(
            old.join("sessions/s1.json").exists(),
            "the old copy is kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repeat_run_copies_nothing_new_and_a_differing_file_keeps_both_versions() {
        let dir = dir("data-repeat");
        let (old, new) = (dir.join("old"), dir.join("new"));
        populate(&old);
        migrate_data_dir(&old, &new);
        std::fs::write(new.join("history.json"), "[\"новая\"]").unwrap();
        let report = migrate_data_dir(&old, &new);
        assert_eq!(report.copied, 0);
        assert_eq!(report.kept_both, ["history.json"]);
        assert_eq!(
            std::fs::read_to_string(new.join("history.json")).unwrap(),
            "[\"новая\"]"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("history.json.legacy")).unwrap(),
            "[\"hi\"]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_file_leaves_no_marker_so_the_move_is_retried() {
        let dir = dir("data-fail");
        let (old, new) = (dir.join("old"), dir.join("new"));
        populate(&old);
        // На месте файла назначения лежит папка: запись не пройдёт.
        std::fs::create_dir_all(new.join("whitelist.json")).unwrap();
        let report = migrate_data_dir(&old, &new);
        assert!(!report.failed.is_empty(), "{report:?}");
        assert!(!new.join(super::super::paths::DATA_MARKER).exists());
        std::fs::remove_dir_all(new.join("whitelist.json")).unwrap();
        let report = migrate_data_dir(&old, &new);
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(
            std::fs::read_to_string(new.join("whitelist.json")).unwrap(),
            "[]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Несколько процессов стартуют разом: файл целый, ключи на месте, копия одна.
    #[test]
    fn several_starts_at_once_neither_corrupt_the_file_nor_lose_a_key() {
        let (dir, path) = setup("race", LEGACY);
        let (backend, store) = store();
        let store = std::sync::Arc::new(store);
        let threads: Vec<_> = (0..5)
            .map(|_| {
                let (path, store) = (path.clone(), std::sync::Arc::clone(&store));
                std::thread::spawn(move || run(&path, &store).unwrap())
            })
            .collect();
        let reports: Vec<Report> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            reports.iter().filter(|r| r.wrote).count(),
            1,
            "only one start edits"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(no_plaintext(&text), "{text}");
        assert!(super::super::parse(&text).is_ok());
        assert_eq!(backend.data.lock().unwrap().len(), 3);
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), LEGACY);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
