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
            WriteError::Io(m) | WriteError::Refused(m) => Self::Io(m),
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

/// Старый конфиг остаётся на месте ради отката: старый бинарь читает именно его. Когда ключи
/// подтверждённо в хранилище, в нём затираются только значения секретов, настройки целы.
/// Старый бинарь после отката увидит настройки и попросит токен заново.
/// Возвращает `true`, если файл переписан. Сомнение в чём-либо — файл не трогается.
pub fn scrub_legacy_config(legacy: &Path, new: &Path, store: &SecretStore) -> bool {
    let (Ok(old_text), Ok(new_text)) =
        (crate::util::read_text(legacy), crate::util::read_text(new))
    else {
        return false;
    };
    let Ok(mut old_doc) = old_text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    let secrets = fields::plaintext_slots(&mut old_doc);
    if secrets.is_empty() {
        return false;
    }
    let Ok(mut new_doc) = new_text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    if fields::contains_inline_secrets(&new_text) || super::parse(&new_text).is_err() {
        return false;
    }
    // Каждый ключ старого файла: в новом на него есть ссылка, и хранилище отдаёт то же значение.
    let new_ids: Vec<String> = fields::reference_slots(&mut new_doc)
        .into_iter()
        .filter_map(
            |(_, reference)| match crate::secrets::RefKind::parse(&reference) {
                Ok(crate::secrets::RefKind::Keyring(id)) => Some(id),
                _ => None,
            },
        )
        .collect();
    for (slot, value) in &secrets {
        let id = slot.secret_id();
        let confirmed = new_ids.contains(&id)
            && matches!(store.get(&id), Ok(Some(stored)) if stored.matches(value));
        if !confirmed {
            return false;
        }
    }
    for (slot, _) in &secrets {
        fields::blank(&mut old_doc, slot);
    }
    let blanked = old_doc.to_string();
    if super::parse(&blanked).is_err() {
        return false;
    }
    write::write_file(legacy, &blanked).is_ok()
}

// ── старая папка данных ─────────────────────────────────────────────────

/// Явный список переносимого. Всё остальное остаётся в старой папке нетронутым: на Windows и
/// macOS старый конфиг лежит прямо в папке данных, и «скопировать всё, кроме» унесло бы ключи.
/// Конфиг и его копии сюда не входят никогда: они едут отдельным шагом через `migrate::run`.
const MIGRATED_FILES: [&str; 5] = [
    "history.json",
    "whitelist.json",
    "checkpoints.jsonl",
    "checkpoints-undone.jsonl",
    // Серверы MCP — данные пользователя; env/headers в нём закрыты от модели (`protect`).
    "mcp.json",
];
const MIGRATED_DIRS: [&str; 5] = [
    "sessions",
    "checkpoints",
    "tool-output",
    "exports",
    "skills",
];
/// Перекачивается само или это журналы: не переносится и не считается «оставленным».
const REBUILDABLE: [&str; 3] = ["models", "semantic", "provider_models.json"];

#[derive(Debug, Default)]
pub struct DataReport {
    pub copied: usize,
    pub already_there: usize,
    /// Файл есть в обоих местах и различается: побеждает более свежий, другой лежит рядом
    /// как `.legacy`. Здесь только то, что сохранено впервые.
    pub kept_both: Vec<String>,
    /// Записи старой папки вне списка: остались на месте, не копировались.
    pub left_behind: Vec<String>,
    pub failed: Vec<String>,
}

fn skip_name(name: &str) -> bool {
    name.ends_with(".log")
        || name.ends_with(".tmp")
        || name.ends_with(".lock")
        || name.contains(".legacy")
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
        if path.is_dir() {
            walk_files(root, &path, out);
        } else if !skip_name(&entry.file_name().to_string_lossy()) {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
}

/// Файлы к переносу по списку и названия остальных записей верхнего уровня.
fn plan_data_files(old: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut left_behind = Vec::new();
    let Ok(entries) = std::fs::read_dir(old) else {
        return (files, left_behind);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() && MIGRATED_DIRS.contains(&name.as_str()) {
            walk_files(old, &path, &mut files);
        } else if path.is_file() && MIGRATED_FILES.contains(&name.as_str()) {
            files.push(PathBuf::from(&name));
        } else if !REBUILDABLE.contains(&name.as_str())
            && !skip_name(&name)
            && !is_config_file(&name)
        {
            left_behind.push(name);
        }
    }
    left_behind.sort();
    (files, left_behind)
}

/// Кладёт проигравшую версию рядом (`имя.legacy`, `имя.legacy.2`, …). Такая же уже лежит —
/// ничего не добавляет, чтобы повторный запуск не плодил копии. `true` — сохранено впервые.
fn keep_aside(target: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    let base = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 1u32.. {
        let name = if n == 1 {
            format!("{base}.legacy")
        } else {
            format!("{base}.legacy.{n}")
        };
        let side = target.with_file_name(name);
        match std::fs::read(&side) {
            Ok(existing) if existing == bytes => return Ok(false),
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::util::atomic_write(&side, bytes)?;
                return Ok(true);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the counter is unbounded")
}

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Конфиг и его копия едут отдельным шагом (`copy_legacy_config`): в «оставленном» им не место,
/// иначе заметка пугает ключами, которых в старом файле к этому моменту уже нет.
fn is_config_file(name: &str) -> bool {
    let base = name.strip_suffix(".pre-gateway").unwrap_or(name);
    base == "config.toml"
}

/// Когда последний раз начинался перенос: по метке. Метка старого формата без времени —
/// по времени её файла. `None` — метки нет.
pub fn marker_started(new: &Path) -> Option<std::time::SystemTime> {
    let marker = new.join(super::paths::DATA_MARKER);
    let started = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("started_ms").and_then(serde_json::Value::as_u64))
        .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms));
    started.or_else(|| modified(&marker))
}

/// Есть ли в старой папке переносимые файлы, изменённые после начала прошлого переноса:
/// их писал запущенный старый бинарь, и они не должны пропасть.
pub fn old_has_news(old: &Path, since: std::time::SystemTime) -> bool {
    let (files, _) = plan_data_files(old);
    files
        .iter()
        .any(|relative| modified(&old.join(relative)).is_some_and(|time| time > since))
}

/// Копирует данные из старой папки в новую. Старая остаётся нетронутой.
/// Метка в новой папке ставится только после полного успеха: иначе перенос повторится.
#[cfg(test)]
pub fn migrate_data_dir(old: &Path, new: &Path) -> DataReport {
    migrate_data_dir_ticking(old, new, &|| {})
}

/// То же, с пульсом после каждого файла: держатель замка сообщает, что жив.
pub fn migrate_data_dir_ticking(old: &Path, new: &Path, tick: &dyn Fn()) -> DataReport {
    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let mut report = DataReport::default();
    let (files, left_behind) = plan_data_files(old);
    report.left_behind = left_behind;
    for relative in files {
        tick();
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
        let mut existing_newer = false;
        match std::fs::read(&target) {
            Ok(existing) if existing == bytes => {
                report.already_there += 1;
                continue;
            }
            Ok(existing) => {
                // Две разные версии: свежая по времени изменения остаётся в работе, вторая
                // сохраняется рядом. Время неизвестно — в работе остаётся то, что уже лежит.
                let old_is_newer = match (modified(&source), modified(&target)) {
                    (Some(old_time), Some(new_time)) => old_time > new_time,
                    _ => false,
                };
                let loser = if old_is_newer { &existing } else { &bytes };
                match keep_aside(&target, loser) {
                    Ok(true) => report.kept_both.push(shown.clone()),
                    Ok(false) => {}
                    Err(e) => {
                        report.failed.push(format!(
                            "{shown}: cannot keep the other version ({})",
                            e.kind()
                        ));
                        continue;
                    }
                }
                if !old_is_newer {
                    existing_newer = true;
                }
            }
            Err(_) => {}
        }
        if existing_newer {
            continue;
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
            "left_behind": report.left_behind,
            "started_ms": started_ms,
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

    /// Старый файл не удаляется: откат на прежнюю версию находит настройки и просит токен заново.
    #[test]
    fn the_old_config_keeps_its_settings_and_loses_only_the_keys_once_they_are_safe() {
        let dir = dir("retire");
        let old = dir.join("old.toml");
        let new = dir.join("new.toml");
        std::fs::write(&old, LEGACY).unwrap();
        std::fs::write(&new, LEGACY).unwrap();
        let (backend, store) = store();
        assert!(
            !scrub_legacy_config(&old, &new, &store),
            "new still holds plaintext: the old file is untouched"
        );
        assert_eq!(std::fs::read_to_string(&old).unwrap(), LEGACY);
        run(&new, &store).unwrap();

        // Хранилище не подтверждает значение — не трогаем.
        backend.mode(Mode::Differs);
        let fresh = SecretStore::new(backend.clone());
        assert!(!scrub_legacy_config(&old, &new, &fresh));
        assert_eq!(std::fs::read_to_string(&old).unwrap(), LEGACY);
        backend.mode(Mode::Healthy);

        assert!(scrub_legacy_config(&old, &new, &store));
        let text = std::fs::read_to_string(&old).unwrap();
        assert!(no_plaintext(&text), "{text}");
        assert!(
            text.contains("# мой конфиг") && text.contains("# DeepSeek"),
            "{text}"
        );
        assert!(
            text.contains("model = \"qwen\"") && text.contains("[[providers]]"),
            "{text}"
        );
        // Старый бинарь прочтёт файл: токена нет, настройки на месте.
        let parsed = super::super::parse(&text).unwrap();
        assert!(parsed.provider.token.is_empty());
        assert_eq!(parsed.providers.len(), 2);
        assert_eq!(parsed.providers[0].api_key, None);
        assert!(!fields::contains_inline_secrets(&text));
        // Второй раз молчит: нечего затирать.
        assert!(!scrub_legacy_config(&old, &new, &store));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_secretless_old_config_is_never_rewritten() {
        let dir = dir("keepold");
        let old = dir.join("old.toml");
        let new = dir.join("new.toml");
        std::fs::write(&old, "[ui]\ntheme = \"x\"\n").unwrap();
        std::fs::write(&new, "[ui]\ntheme = \"x\"\n").unwrap();
        let (_, store) = store();
        assert!(!scrub_legacy_config(&old, &new, &store));
        assert_eq!(
            std::fs::read_to_string(&old).unwrap(),
            "[ui]\ntheme = \"x\"\n"
        );
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

    /// На Windows и macOS старый конфиг лежит прямо в папке данных. Он, его копии и всё
    /// незнакомое остаются в старой папке; в новую не попадает ничего, что может держать ключи.
    #[test]
    fn a_config_inside_the_old_data_folder_is_never_copied() {
        let dir = dir("data-config");
        let (old, new) = (dir.join("old"), dir.join("new"));
        populate(&old);
        std::fs::write(old.join("config.toml"), LEGACY).unwrap();
        std::fs::write(old.join("config.toml.pre-gateway"), LEGACY).unwrap();
        std::fs::write(old.join("mcp.json.bak-craws"), "{\"env\":{\"K\":\"v\"}}").unwrap();
        std::fs::write(old.join("notes.txt"), "token = abc").unwrap();
        std::fs::create_dir_all(old.join("secrets")).unwrap();
        std::fs::write(old.join("secrets").join("key.bin"), "k").unwrap();
        let report = migrate_data_dir(&old, &new);
        assert!(report.failed.is_empty(), "{report:?}");
        for left in [
            "config.toml",
            "config.toml.pre-gateway",
            "mcp.json.bak-craws",
            "notes.txt",
            "secrets",
        ] {
            assert!(!new.join(left).exists(), "{left} must not be copied");
            assert!(old.join(left).exists(), "{left} stays in the old folder");
        }
        // Конфиг едет отдельным шагом: заметка «оставлено, может хранить ключи» его не называет.
        assert_eq!(
            report.left_behind,
            ["mcp.json.bak-craws", "notes.txt", "secrets"],
            "{report:?}"
        );
        // Ничто в новой папке не держит ключ.
        fn any_key(dir: &Path) -> bool {
            std::fs::read_dir(dir).unwrap().flatten().any(|e| {
                let p = e.path();
                if p.is_dir() {
                    any_key(&p)
                } else {
                    std::fs::read_to_string(&p).is_ok_and(|t| t.contains("ds-token-1234567890"))
                }
            })
        }
        assert!(!any_key(&new));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_kind_of_user_data_in_the_list_is_moved() {
        let dir = dir("data-list");
        let (old, new) = (dir.join("old"), dir.join("new"));
        for d in [
            "sessions",
            "checkpoints",
            "tool-output",
            "exports",
            "skills",
        ] {
            std::fs::create_dir_all(old.join(d).join("nested")).unwrap();
            std::fs::write(old.join(d).join("nested").join("f.txt"), d).unwrap();
        }
        for f in [
            "history.json",
            "whitelist.json",
            "checkpoints.jsonl",
            "checkpoints-undone.jsonl",
            "mcp.json",
        ] {
            std::fs::write(old.join(f), f).unwrap();
        }
        let report = migrate_data_dir(&old, &new);
        assert!(
            report.failed.is_empty() && report.left_behind.is_empty(),
            "{report:?}"
        );
        assert_eq!(report.copied, 10);
        assert_eq!(
            std::fs::read_to_string(new.join("skills/nested/f.txt")).unwrap(),
            "skills"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(
            report.left_behind.is_empty(),
            "rebuildable data is not 'left behind'"
        );
        assert!(new.join(super::super::paths::DATA_MARKER).exists());
        assert!(
            old.join("sessions/s1.json").exists(),
            "the old copy is kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_age(path: &Path, newer_than: &Path) {
        let later = std::fs::metadata(newer_than).unwrap().modified().unwrap()
            + std::time::Duration::from_secs(120);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(later).unwrap();
    }

    /// Свежая копия по времени изменения побеждает, другая сохраняется рядом; повтор ничего не плодит.
    #[test]
    fn the_newer_copy_wins_and_the_other_is_kept_aside_once() {
        let dir = dir("data-newer");
        let (old, new) = (dir.join("old"), dir.join("new"));
        populate(&old);
        migrate_data_dir(&old, &new);
        // Новая папка получила свою версию позже копии, старая — ещё позже (старый бинарь писал дальше).
        std::fs::write(new.join("history.json"), "[\"новая\"]").unwrap();
        std::fs::write(old.join("history.json"), "[\"свежая старая\"]").unwrap();
        set_age(&old.join("history.json"), &new.join("history.json"));
        let first = migrate_data_dir(&old, &new);
        assert_eq!(first.kept_both, ["history.json"]);
        assert_eq!(
            std::fs::read_to_string(new.join("history.json")).unwrap(),
            "[\"свежая старая\"]",
            "the fresher old file wins"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("history.json.legacy")).unwrap(),
            "[\"новая\"]",
            "nothing is lost"
        );
        let second = migrate_data_dir(&old, &new);
        assert!(second.kept_both.is_empty(), "{second:?}");
        assert!(!new.join("history.json.legacy.2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_the_new_copy_is_newer_the_old_one_is_kept_aside_and_repeats_add_nothing() {
        let dir = dir("data-repeat");
        let (old, new) = (dir.join("old"), dir.join("new"));
        populate(&old);
        migrate_data_dir(&old, &new);
        std::fs::write(new.join("history.json"), "[\"новая\"]").unwrap();
        set_age(&new.join("history.json"), &old.join("history.json"));
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
        for _ in 0..3 {
            assert!(migrate_data_dir(&old, &new).kept_both.is_empty());
        }
        assert!(!new.join("history.json.legacy.2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Два разных проигравших подряд не затирают друг друга.
    #[test]
    fn a_second_different_loser_gets_its_own_aside_file() {
        let dir = dir("data-two-losers");
        let target = dir.join("x.json");
        assert!(keep_aside(&target, b"one").unwrap());
        assert!(keep_aside(&target, b"two").unwrap());
        assert!(!keep_aside(&target, b"one").unwrap());
        assert_eq!(std::fs::read(dir.join("x.json.legacy")).unwrap(), b"one");
        assert_eq!(std::fs::read(dir.join("x.json.legacy.2")).unwrap(), b"two");
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

    /// Запись без модели (мастер позволяет её пропустить) переезжает в аккаунты целиком.
    #[test]
    fn a_provider_without_a_model_is_mirrored_into_accounts_not_lost() {
        let text = "[[providers]]\nname = \"lmstudio\"\nbase_url = \"http://localhost:1234/v1\"\n";
        let (dir, path) = setup("nomodel", text);
        let (_, store) = store();
        let report = run(&path, &store).unwrap();
        assert!(report.gateway_added, "{report:?}");
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(
            migrated.contains("[[providers]]"),
            "the old entry stays: {migrated}"
        );
        let config = super::super::parse(&migrated).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].id.as_str(), "lmstudio");
        assert!(
            config.accounts[0].model.is_none(),
            "the empty model stays empty"
        );
        assert!(
            !config.gateway.enabled,
            "the old way of working is unchanged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
