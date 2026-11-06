//! Скан папки для `@путь`: что читать и как не плодить задачи. Диск трогает
//! только `FileScan::run` (на `spawn_blocking`); остальное — чистые строки и
//! счётчики, поэтому поведение при быстром наборе проверяется тестами.

use crate::commands::CommandSuggestion;
use crate::util::format_size;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Сколько подсказок файлов отдаём: список всё равно листается по восемь.
const MAX_FILE_SUGGESTIONS: usize = 200;

/// Номер запроса скана. Ответ с чужим номером устарел и отбрасывается;
/// ноль не выдаётся — он значит «никто не ждёт».
pub(crate) fn next_scan_generation() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Что и где искать для `@путь`: папка и начало имени (в нижнем регистре).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileScan {
    pub(crate) dir: PathBuf,
    pub(crate) prefix: String,
}

impl FileScan {
    /// Разобрать `@путь` относительно рабочей папки. Только строки, без диска.
    pub(crate) fn parse(path_part: &str, cwd: &Path) -> Self {
        let has_separator = path_part.contains('/') || path_part.contains('\\');
        let search_path = if has_separator {
            PathBuf::from(path_part)
        } else {
            cwd.join(path_part)
        };
        let prefix = search_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_lowercase();
        let dir = if has_separator {
            let parent = search_path.parent().unwrap_or(cwd);
            if parent.is_absolute() {
                parent.to_path_buf()
            } else {
                cwd.join(parent)
            }
        } else {
            cwd.to_path_buf()
        };
        Self { dir, prefix }
    }

    /// Прочитать папку. Блокирует на диске — звать только из `spawn_blocking`.
    pub(crate) fn run(&self) -> Vec<CommandSuggestion> {
        let Ok(read_dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.to_lowercase().starts_with(&self.prefix) {
                continue;
            }
            let path = entry.path();
            // Ссылку приходится разворачивать, обычные записи типа хватает без `stat`.
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => path.is_dir(),
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
            entries.push((name, path, is_dir));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.truncate(MAX_FILE_SUGGESTIONS);
        entries
            .into_iter()
            .map(|(name, path, is_dir)| {
                let description = if is_dir {
                    "dir".to_string()
                } else {
                    format_size(path.metadata().map(|meta| meta.len()).unwrap_or(0))
                };
                CommandSuggestion {
                    name: format!("{name}{}", if is_dir { "/" } else { "" }),
                    description,
                    usage: path.to_string_lossy().to_string(),
                }
            })
            .collect()
    }
}

/// Приказ запустить скан: номер запроса и что читать.
pub(crate) type Launch = (u64, FileScan);

/// Учёт сканов. Одновременно идёт не больше одного: пока он идёт, новые
/// запросы сливаются в один «следующий» (остаётся самый свежий), а результат
/// устаревшего запроса отбрасывается и никогда не показывается.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScanTracker {
    /// Номер запроса, чей ответ ещё годится для показа (0 — никакой).
    wanted: u64,
    /// Номер идущего сейчас скана (0 — нет).
    in_flight: u64,
    /// Самый свежий запрос, что ждёт конца идущего.
    pending: Option<FileScan>,
    /// Запрос, по которому уже есть или идёт ответ: тот же — не повторяем.
    last: Option<FileScan>,
}

impl ScanTracker {
    /// Нужен список для `scan`. Вернёт приказ запуска, если пора начинать.
    pub(crate) fn want(&mut self, scan: FileScan, new_generation: u64) -> Option<Launch> {
        if self.last.as_ref() == Some(&scan) {
            // Стрелки и прочие клавиши не меняют запрос — читать заново не надо.
            return None;
        }
        self.last = Some(scan.clone());
        // Всё, что пришло по прежним запросам, устарело.
        self.wanted = 0;
        if self.in_flight != 0 {
            self.pending = Some(scan);
            return None;
        }
        self.start(scan, new_generation)
    }

    /// Ввод ушёл из `@`-режима: прежние ответы не нужны, а идущий скан
    /// закончится сам и не будет показан.
    pub(crate) fn leave(&mut self) {
        self.wanted = 0;
        self.pending = None;
        self.last = None;
    }

    /// Пришёл ответ скана `generation`. Первое — показывать ли его, второе —
    /// что запустить следом (самый свежий отложенный запрос).
    pub(crate) fn finish(
        &mut self,
        generation: u64,
        new_generation: impl FnOnce() -> u64,
    ) -> (bool, Option<Launch>) {
        if generation == self.in_flight {
            self.in_flight = 0;
        }
        let show = generation == self.wanted && generation != 0;
        let next = self
            .pending
            .take()
            .and_then(|scan| self.start(scan, new_generation()));
        (show, next)
    }

    fn start(&mut self, scan: FileScan, generation: u64) -> Option<Launch> {
        self.wanted = generation;
        self.in_flight = generation;
        Some((generation, scan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-ac-{tag}-{}-{}",
            std::process::id(),
            next_scan_generation()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn scan(prefix: &str) -> FileScan {
        FileScan {
            dir: PathBuf::from("work"),
            prefix: prefix.to_string(),
        }
    }

    #[test]
    fn a_bare_name_is_searched_in_the_workspace() {
        let cwd = Path::new("work");
        let scan = FileScan::parse("Rea", cwd);
        assert_eq!(scan.dir, cwd);
        assert_eq!(scan.prefix, "rea");
    }

    #[test]
    fn a_path_with_a_separator_is_searched_in_its_parent() {
        let cwd = Path::new("work");
        let scan = FileScan::parse("src/ma", cwd);
        assert_eq!(scan.dir, cwd.join("src"));
        assert_eq!(scan.prefix, "ma");
    }

    #[test]
    fn the_scan_finds_files_and_marks_folders() {
        let dir = temp_dir("scan");
        std::fs::write(dir.join("readme.md"), "hello").unwrap();
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join("other.txt"), "x").unwrap();
        let items = FileScan {
            dir: dir.clone(),
            prefix: "r".to_string(),
        }
        .run();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "readme.md");
        let all = FileScan {
            dir: dir.clone(),
            prefix: String::new(),
        }
        .run();
        assert!(
            all.iter()
                .any(|item| item.name == "src/" && item.description == "dir")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_folder_gives_no_suggestions() {
        assert!(scan("").run().is_empty());
    }

    #[test]
    fn scan_numbers_only_grow_and_never_hit_zero() {
        let first = next_scan_generation();
        let second = next_scan_generation();
        assert!(first > 0 && second > first);
    }

    #[test]
    fn the_first_request_starts_a_scan() {
        let mut tracker = ScanTracker::default();
        let launch = tracker.want(scan("a"), 1).expect("launch");
        assert_eq!(launch.0, 1);
        assert_eq!(tracker.finish(1, || 2), (true, None));
    }

    /// Устаревший ответ не показывается, пока ждёт новый запрос.
    #[test]
    fn a_stale_answer_is_dropped_and_the_newest_request_runs_next() {
        let mut tracker = ScanTracker::default();
        tracker.want(scan("a"), 1).expect("launch");
        // Пока идёт первый, ввод менялся трижды: остаётся только последний.
        assert!(tracker.want(scan("ab"), 2).is_none());
        assert!(tracker.want(scan("abc"), 3).is_none());
        assert!(tracker.want(scan("abcd"), 4).is_none());
        let (show, next) = tracker.finish(1, || 5);
        assert!(!show, "ответ на «a» устарел");
        let (generation, request) = next.expect("свежий запрос");
        assert_eq!((generation, request.prefix.as_str()), (5, "abcd"));
        // Его ответ — годный.
        assert_eq!(tracker.finish(5, || 6), (true, None));
    }

    /// Не больше одного идущего скана, как ни стучи по `@`, Backspace, `@`.
    #[test]
    fn leaving_and_re_entering_does_not_start_a_second_scan_at_once() {
        let mut tracker = ScanTracker::default();
        tracker.want(scan("a"), 1).expect("launch");
        for generation in 2..10 {
            tracker.leave();
            assert!(
                tracker.want(scan("a"), generation).is_none(),
                "скан уже идёт — запрос откладывается, а не запускается"
            );
        }
        let (show, next) = tracker.finish(1, || 100);
        assert!(!show);
        assert_eq!(next.map(|(generation, _)| generation), Some(100));
    }

    #[test]
    fn the_same_request_is_not_repeated() {
        let mut tracker = ScanTracker::default();
        tracker.want(scan("a"), 1).expect("launch");
        assert!(
            tracker.want(scan("a"), 2).is_none(),
            "стрелка влево не повод читать диск"
        );
        assert_eq!(tracker.finish(1, || 3), (true, None));
        assert!(tracker.want(scan("a"), 4).is_none(), "и после ответа тоже");
        assert!(
            tracker.want(scan("b"), 5).is_some(),
            "другой запрос — читаем"
        );
    }

    #[test]
    fn an_answer_after_leaving_the_mode_is_not_shown() {
        let mut tracker = ScanTracker::default();
        tracker.want(scan("a"), 1).expect("launch");
        tracker.leave();
        assert_eq!(tracker.finish(1, || 2), (false, None));
        // И следующий вход снова может читать.
        assert!(tracker.want(scan("a"), 3).is_some());
    }
}
