//! Снимки файлов перед правкой и откат по `/undo`.
//!
//! Инструменты `edit`/`write` пишут необратимо, а подтверждение в модалке
//! ловит только явно опасную команду — не промах якорем и не перезапись не
//! того файла.
//!
//! Журнал **append-only**, и это не стилистика. Приложение живёт не в одном
//! процессе: рядом с TUI бегают дочерние процессы `exec`/`scenario`, гоняющие
//! настоящие инструменты. Первая версия перезаписывала журнал целиком под
//! внутрипроцессным мьютексом — на двух процессах записи затирали друг друга,
//! и запись начинала указывать на копию **чужого** файла. Дописывание строки
//! и уникальные имена копий убирают этот класс целиком.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static SHARED: OnceLock<Store> = OnceLock::new();

const JOURNAL: &str = "checkpoints.jsonl";
const UNDONE: &str = "checkpoints-undone.jsonl";
const BLOBS: &str = "checkpoints";

/// Сколько записей храним. Дальше журнал переписывается, а лишние копии
/// вычищаются: откатывают почти всегда последнюю правку.
const MAX_ENTRIES: usize = 200;

/// Потолок на копию. Выше него снимок не делаем: гигабайт сгенерированного
/// файла в папке данных дороже возможности его откатить.
const MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024;

/// Имена, содержимое которых не копируем никуда и никогда. Копия `.env` в
/// папке данных переживает и ротацию ключа, и удаление самого файла.
const SECRET_PATTERNS: &[&str] = &[
    ".env",
    ".netrc",
    "credentials",
    "id_rsa",
    "id_ed25519",
    ".pem",
    ".key",
    ".p12",
    ".pfx",
];

/// Чем был файл до правки.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Before {
    /// Файла не было — откат его удалит.
    Absent,
    /// Копия прежнего содержимого.
    Blob { name: String },
    /// Снимок сознательно не сделан. Отдельный случай, а не «файла не было»:
    /// перепутать их значит удалить существующий файл при откате.
    Skipped { reason: String },
}

/// Одна запись журнала.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Уникален между процессами: время, pid и счётчик.
    pub id: String,
    /// Канонический путь — тот же, по которому шла запись.
    pub path: PathBuf,
    pub before: Before,
    /// Инструмент: `edit` или `write`.
    pub tool: String,
    pub at: String,
    /// Размер файла сразу после правки. Расхождение означает, что файл трогал
    /// кто-то ещё, и откат затёр бы чужую работу.
    pub after_len: u64,
}

impl Checkpoint {
    /// Строка для `/undo list`; её же печатает откат.
    pub fn describe(&self) -> String {
        let kind = match &self.before {
            Before::Absent => "created".to_string(),
            Before::Blob { .. } => "modified".to_string(),
            Before::Skipped { reason } => format!("modified, no snapshot ({reason})"),
        };
        format!(
            "{} · {kind} by {} · {}",
            self.at,
            self.tool,
            self.path.display()
        )
    }

    fn undoable(&self) -> bool {
        !matches!(self.before, Before::Skipped { .. })
    }
}

/// Журнал в конкретной папке. Корень — параметр, а не константа: иначе тесты
/// писали бы в настоящую папку данных пользователя.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Указать папку журнала. Зовётся один раз из `main`; повторный вызов
    /// игнорируется.
    pub fn init(root: PathBuf) {
        let _ = SHARED.set(Store::at(root));
    }

    /// Журнал приложения. Снимок делает инструмент, а откатывает команда —
    /// общий владелец у них только процесс.
    ///
    /// Без `init` уходит во временную папку, а не в папку данных: под тестами
    /// инструменты зовут `record` по-настоящему, и прогон дважды успел
    /// насорить в настоящей папке пользователя.
    pub fn shared() -> &'static Store {
        SHARED.get_or_init(|| {
            Store::at(
                std::env::temp_dir().join(format!("pooprusteek-scratch-{}", std::process::id())),
            )
        })
    }

    fn journal(&self) -> PathBuf {
        self.root.join(JOURNAL)
    }

    fn undone_path(&self) -> PathBuf {
        self.root.join(UNDONE)
    }

    fn blobs(&self) -> PathBuf {
        self.root.join(BLOBS)
    }

    /// Дописать строку. Открытие в режиме `append` — единственная операция,
    /// которую два процесса делают безопасно без общей блокировки.
    fn append_line(&self, path: &Path, line: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        // Одним вызовом вместе с переводом строки: два отдельных `write_all`
        // пускали чужой поток между ними и рвали строку пополам.
        file.write_all(format!("{line}\n").as_bytes())
    }

    /// Записи журнала. Битая строка пропускается по одной — целый журнал из-за
    /// неё терять нельзя, а раньше терялся: разбор всего файла шёл через
    /// `.ok()`, и обрезанный JSON читался как «истории нет».
    pub fn list(&self) -> Vec<Checkpoint> {
        let Ok(text) = std::fs::read_to_string(self.journal()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Checkpoint>(line) {
                Ok(entry) => out.push(entry),
                Err(error) => {
                    tracing::warn!(
                        "checkpoint journal line {} is unreadable: {error}",
                        number + 1
                    )
                }
            }
        }
        out
    }

    fn undone(&self) -> std::collections::HashSet<String> {
        std::fs::read_to_string(self.undone_path())
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Записи, которые ещё можно откатить, от новых к старым.
    pub fn pending(&self) -> Vec<Checkpoint> {
        let undone = self.undone();
        let mut entries: Vec<Checkpoint> = self
            .list()
            .into_iter()
            .filter(|e| !undone.contains(&e.id))
            .collect();
        entries.reverse();
        entries
    }

    /// Снять копию **после** успешной записи. Именно после: раньше снимок шёл
    /// до, и отказанная правка защищённого файла всё равно оставляла запись в
    /// журнале и копировала его содержимое в папку копий.
    ///
    /// Ошибку не поднимаем — неудачный снимок не должен отменять уже сделанную
    /// правку, — но возвращаем её текстом, чтобы вызывающий сказал человеку.
    pub fn record(&self, path: &Path, tool: &str, previous: Option<&[u8]>) -> Option<String> {
        match self.record_inner(path, tool, previous) {
            Ok(()) => None,
            Err(error) => {
                tracing::warn!("checkpoint for {} failed: {error}", path.display());
                Some(format!("(not undoable: {error})"))
            }
        }
    }

    fn record_inner(&self, path: &Path, tool: &str, previous: Option<&[u8]>) -> Result<(), String> {
        let id = next_id();
        let before = match previous {
            None => Before::Absent,
            Some(_) if looks_like_a_secret(path) => Before::Skipped {
                reason: "looks like a secret".to_string(),
            },
            Some(bytes) if bytes.len() as u64 > MAX_BLOB_BYTES => Before::Skipped {
                reason: format!("larger than {} MiB", MAX_BLOB_BYTES / 1024 / 1024),
            },
            Some(bytes) => {
                let name = format!("{id}.bak");
                write_blob(&self.blobs().join(&name), bytes)
                    .map_err(|e| format!("could not store the snapshot: {e}"))?;
                Before::Blob { name }
            }
        };

        let entry = Checkpoint {
            id,
            path: path.to_path_buf(),
            before,
            tool: tool.to_string(),
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            after_len: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        };
        let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
        self.append_line(&self.journal(), &line)
            .map_err(|e| format!("could not write the journal: {e}"))?;
        self.prune();
        Ok(())
    }

    /// Урезать журнал и снести копии, на которые больше никто не смотрит.
    /// Переписывание редкое, и гонку с чужим дописыванием мы принимаем: цена
    /// — потерянная запись о правке, а не потерянный файл.
    fn prune(&self) {
        let entries = self.list();
        if entries.len() <= MAX_ENTRIES * 2 {
            return;
        }
        let keep = &entries[entries.len() - MAX_ENTRIES..];
        let kept: std::collections::HashSet<&str> = keep
            .iter()
            .filter_map(|e| match &e.before {
                Before::Blob { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let body: String = keep
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .map(|line| line + "\n")
            .collect();
        if crate::util::atomic_write(&self.journal(), body.as_bytes()).is_err() {
            return;
        }
        // Копии сносим ПОСЛЕ записи журнала: осиротевший файл дешевле, чем
        // запись, указывающая в пустоту.
        if let Ok(dir) = std::fs::read_dir(self.blobs()) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !kept.contains(name.as_str()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    /// Что откатит следующий `/undo`. Отдельно от самого отката, чтобы
    /// показать это в подтверждении.
    pub fn next_undo(&self) -> Result<Checkpoint, String> {
        match self.pending().into_iter().next() {
            None => Err("Nothing to undo — no file edits recorded yet.".to_string()),
            Some(entry) if !entry.undoable() => Err(format!(
                "The last change to {} has no snapshot, so it cannot be undone ({}). Use `/undo skip` to step past it.",
                entry.path.display(),
                match &entry.before {
                    Before::Skipped { reason } => reason.clone(),
                    _ => String::new(),
                }
            )),
            Some(entry) => Ok(entry),
        }
    }

    /// Пометить запись как обработанную, не трогая файл.
    pub fn skip_next(&self) -> Result<String, String> {
        let Some(entry) = self.pending().into_iter().next() else {
            return Err("Nothing to skip — the journal is empty.".to_string());
        };
        self.mark_undone(&entry.id)?;
        Ok(format!("Skipped {}", entry.describe()))
    }

    fn mark_undone(&self, id: &str) -> Result<(), String> {
        self.append_line(&self.undone_path(), id)
            .map_err(|e| format!("Failed to update the undo journal: {e}"))
    }

    /// Откатить последнюю правку. Блокирующий ввод-вывод — звать с
    /// `spawn_blocking`, не с цикла событий.
    pub fn undo_last(&self) -> Result<String, String> {
        let entry = self.next_undo()?;

        // Сначала достаём данные и проверяем обстановку, и только потом
        // трогаем файл: на полпути он не должен остаться ни старым, ни новым.
        let restore = match &entry.before {
            Before::Blob { name } => {
                Some(std::fs::read(self.blobs().join(name)).map_err(|e| {
                    format!("The snapshot for {} is gone: {e}", entry.path.display())
                })?)
            }
            Before::Absent => None,
            Before::Skipped { .. } => unreachable!("next_undo отсеивает записи без снимка"),
        };

        // Файл изменился с момента правки — значит его трогал кто-то ещё
        // (человек в редакторе, другой ход), и откат затёр бы чужую работу.
        let current_len = std::fs::metadata(&entry.path).map(|m| m.len()).ok();
        if current_len != Some(entry.after_len) {
            return Err(format!(
                "{} changed after the edit was made — refusing to undo it. Check the file, then `/undo skip` if you still want to move past this entry.",
                entry.path.display()
            ));
        }

        let shown = entry.path.display().to_string();
        let summary = match restore {
            Some(bytes) => {
                crate::safe_write::write_preserving(&entry.path, &shown, &bytes)?;
                format!("Restored {shown}")
            }
            None => {
                crate::safe_write::refuse_protected(&entry.path, &shown)?;
                std::fs::remove_file(&entry.path)
                    .map_err(|e| format!("Failed to remove {shown}: {e}"))?;
                format!("Removed {shown} (it did not exist before the edit)")
            }
        };
        self.mark_undone(&entry.id)?;
        Ok(format!("{summary}\n{}", entry.describe()))
    }
}

/// Уникальный идентификатор записи. Время плюс pid плюс счётчик: два процесса
/// не должны выдать одно имя копии, иначе одна затрёт другую.
fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:039}-{}-{:04}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed) % 10_000
    )
}

/// Похоже ли имя на файл с секретом. Грубо и намеренно широко: ложное
/// срабатывание стоит одной невозможной отмены, пропуск — копии ключа.
fn looks_like_a_secret(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    SECRET_PATTERNS
        .iter()
        .any(|needle| name.starts_with(needle) || name.ends_with(needle))
}

/// Копия на диске. На unix — только для владельца: в ней лежит прежнее
/// содержимое чужих файлов.
fn write_blob(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::util::atomic_write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn record(path: &Path, tool: &str, previous: Option<&[u8]>) -> Option<String> {
    Store::shared().record(path, tool, previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Свой корень на тест: общий журнал сделал бы тесты зависимыми друг от
    /// друга, а `Store::shared()` писал бы в настоящую папку пользователя.
    struct Fixture {
        store: Store,
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pooprusteek_checkpoint_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(dir.join("work")).unwrap();
            Self {
                store: Store::at(dir.join("data")),
                dir,
            }
        }

        fn file(&self, name: &str, content: &str) -> PathBuf {
            let path = self.dir.join("work").join(name);
            std::fs::write(&path, content).unwrap();
            path
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join("work").join(name)
        }

        /// Записать файл и сразу снять снимок — то, что делает инструмент.
        fn edit(&self, path: &PathBuf, new: &str) {
            let previous = std::fs::read(path).ok();
            std::fs::write(path, new).unwrap();
            self.store.record(path, "edit", previous.as_deref());
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn an_edit_can_be_undone() {
        let fx = Fixture::new("undo");
        let path = fx.file("file.txt", "before\n");
        fx.edit(&path, "after\n");

        let report = fx.store.undo_last().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
        assert!(report.contains("Restored"), "{report}");
    }

    /// Регрессия на блокер первой версии: она снимала запись с копии журнала,
    /// не удаляя её с диска, и дописывала обратный снимок сверху. Второй
    /// `/undo` делал redo первой правки, а до второго файла было не добраться.
    #[test]
    fn two_edits_undo_in_reverse_order() {
        let fx = Fixture::new("two");
        let a = fx.file("a.txt", "a1\n");
        let b = fx.file("b.txt", "b1\n");
        fx.edit(&a, "a2\n");
        fx.edit(&b, "b2\n");

        fx.store.undo_last().unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b1\n");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "a2\n",
            "рано откатили a"
        );

        fx.store.undo_last().unwrap();
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "a1\n",
            "до a не дошли"
        );
    }

    #[test]
    fn undoing_a_creation_removes_the_file() {
        let fx = Fixture::new("created");
        let path = fx.path("new.txt");
        std::fs::write(&path, "content\n").unwrap();
        fx.store.record(&path, "write", None);

        let report = fx.store.undo_last().unwrap();
        assert!(!path.exists(), "созданный файл не удалён");
        assert!(report.contains("Removed"), "{report}");
    }

    #[test]
    fn a_file_changed_after_the_edit_is_not_clobbered() {
        // Человек правил файл в редакторе после агента — откат затёр бы его.
        let fx = Fixture::new("changed");
        let path = fx.file("file.txt", "before\n");
        fx.edit(&path, "agent\n");
        std::fs::write(&path, "human wrote this and more\n").unwrap();

        let error = fx.store.undo_last().unwrap_err();
        assert!(error.contains("changed after the edit"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "human wrote this and more\n"
        );
    }

    #[test]
    fn undoing_a_creation_the_user_has_since_filled_is_refused() {
        let fx = Fixture::new("refilled");
        let path = fx.path("new.txt");
        std::fs::write(&path, "agent\n").unwrap();
        fx.store.record(&path, "write", None);
        std::fs::write(&path, "the user's own notes\n").unwrap();

        assert!(fx.store.undo_last().unwrap_err().contains("changed after"));
        assert!(path.exists(), "чужая работа удалена");
    }

    #[test]
    fn an_empty_journal_says_so_instead_of_failing_silently() {
        let fx = Fixture::new("empty");
        assert!(
            fx.store
                .undo_last()
                .unwrap_err()
                .contains("Nothing to undo")
        );
    }

    #[test]
    fn a_secret_file_is_never_copied_into_the_data_directory() {
        let fx = Fixture::new("secret");
        let path = fx.file(".env", "TOKEN=super-secret\n");
        fx.edit(&path, "TOKEN=changed\n");

        let entry = &fx.store.pending()[0];
        assert!(
            matches!(&entry.before, Before::Skipped { .. }),
            "{:?}",
            entry.before
        );
        // Ни одного файла копий: содержимое не покинуло рабочую папку.
        let blobs = std::fs::read_dir(fx.store.blobs())
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(blobs, 0, "копия секрета всё-таки создана");
        assert!(
            fx.store
                .undo_last()
                .unwrap_err()
                .contains("cannot be undone")
        );
    }

    #[test]
    fn skip_steps_past_an_entry_without_a_snapshot() {
        let fx = Fixture::new("skip");
        let plain = fx.file("file.txt", "v1\n");
        fx.edit(&plain, "v2\n");
        let secret = fx.file(".env", "A=1\n");
        fx.edit(&secret, "A=2\n");

        assert!(fx.store.undo_last().is_err());
        fx.store.skip_next().unwrap();
        // За пропущенной записью снова доступна обычная.
        fx.store.undo_last().unwrap();
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "v1\n");
    }

    #[test]
    fn a_broken_journal_line_does_not_destroy_the_rest() {
        let fx = Fixture::new("broken");
        let path = fx.file("file.txt", "v1\n");
        fx.edit(&path, "v2\n");
        fx.store
            .append_line(&fx.store.journal(), "{not json at all")
            .unwrap();
        fx.edit(&path, "v3\n");

        // Две настоящие записи уцелели, битая пропущена.
        assert_eq!(fx.store.list().len(), 2);
        fx.store.undo_last().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2\n");
    }

    #[test]
    fn a_restored_file_keeps_binary_content_intact() {
        let fx = Fixture::new("binary");
        let path = fx.path("blob.bin");
        let original = [0xffu8, 0xfe, 0x00, 0x41];
        std::fs::write(&path, original).unwrap();
        fx.store.record(&path, "write", Some(&original));
        // Правка после снимка: длина должна совпасть, поэтому пишем столько же.
        std::fs::write(&path, [0x41u8, 0x42, 0x43, 0x44]).unwrap();

        // Снимок записан с `after_len` от исходного файла — он и есть текущий.
        let entry = &fx.store.pending()[0];
        assert_eq!(entry.after_len, 4);
        fx.store.undo_last().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn a_missing_blob_leaves_the_file_alone_instead_of_truncating_it() {
        let fx = Fixture::new("lost_blob");
        let path = fx.file("file.txt", "before\n");
        fx.edit(&path, "after!\n");
        let Before::Blob { name } = fx.store.pending()[0].before.clone() else {
            panic!("ожидалась копия");
        };
        std::fs::remove_file(fx.store.blobs().join(name)).unwrap();

        let error = fx.store.undo_last().unwrap_err();
        assert!(error.contains("snapshot for"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after!\n");
        // Запись не потеряна: копию можно вернуть и повторить.
        assert_eq!(fx.store.pending().len(), 1);
    }

    #[test]
    fn a_multibyte_path_survives_the_round_trip() {
        let fx = Fixture::new("utf8_path");
        let path = fx.file("файл.txt", "было\n");
        fx.edit(&path, "стало\n");

        fx.store.undo_last().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "было\n");
    }

    #[test]
    fn an_oversized_file_is_recorded_without_a_snapshot() {
        let fx = Fixture::new("huge");
        let path = fx.file("big.bin", "x");
        let huge = vec![b'x'; (MAX_BLOB_BYTES + 1) as usize];
        fx.store.record(&path, "write", Some(&huge));
        assert!(matches!(
            &fx.store.pending()[0].before,
            Before::Skipped { .. }
        ));
    }

    #[test]
    fn ids_are_unique_across_rapid_records() {
        let fx = Fixture::new("ids");
        let path = fx.file("file.txt", "x");
        for _ in 0..50 {
            fx.store.record(&path, "edit", Some(b"x"));
        }
        let ids: std::collections::HashSet<String> =
            fx.store.list().into_iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 50, "идентификаторы столкнулись");
    }

    #[test]
    fn concurrent_records_from_several_threads_all_survive() {
        // Дописывание строки, а не перезапись файла: на этом первая версия и
        // ломалась, теряя половину записей и путая копии между файлами.
        let fx = Fixture::new("concurrent");
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let store = &fx.store;
                let dir = fx.dir.join("work");
                scope.spawn(move || {
                    for n in 0..10 {
                        let path = dir.join(format!("w{worker}-{n}.txt"));
                        let body = format!("content of {worker}-{n}");
                        std::fs::write(&path, &body).unwrap();
                        store.record(&path, "edit", Some(body.as_bytes()));
                    }
                });
            }
        });

        let entries = fx.store.list();
        assert_eq!(entries.len(), 40, "записи потерялись");
        // И, главное, каждая копия принадлежит своему файлу.
        for entry in entries {
            let Before::Blob { name } = entry.before else {
                panic!("ожидалась копия");
            };
            let stored = std::fs::read_to_string(fx.store.blobs().join(name)).unwrap();
            let expected = entry.path.file_stem().unwrap().to_string_lossy();
            let expected = expected.trim_start_matches('w').replace('-', "-");
            assert_eq!(
                stored,
                format!("content of {expected}"),
                "копия принадлежит чужому файлу: {:?}",
                entry.path
            );
        }
    }

    #[test]
    fn describe_names_the_file_and_the_tool() {
        let entry = Checkpoint {
            id: "x".to_string(),
            path: PathBuf::from("/repo/src/main.rs"),
            before: Before::Blob {
                name: "x.bak".to_string(),
            },
            tool: "edit".to_string(),
            at: "12:00:00".to_string(),
            after_len: 10,
        };
        let text = entry.describe();
        assert!(text.contains("modified"), "{text}");
        assert!(text.contains("main.rs"), "{text}");
    }
}
