//! Autocomplete: dropdown navigation interception, suggestion refresh (both
//! `/command` and `@file` modes), and acceptance of the selected entry.

use crate::app::events::AppEvent;
use crate::app::{AUTOCOMPLETE_VISIBLE, App, AutocompleteState};
use crate::commands::CommandSuggestion;
use crate::util::format_size;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Сколько подсказок файлов отдаём: список всё равно листается по восемь.
const MAX_FILE_SUGGESTIONS: usize = 200;

/// Номер запроса скана. Ответ с чужим номером устарел и отбрасывается;
/// ноль не выдаётся — он значит «никто не ждёт».
fn next_scan_generation() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Что и где искать для `@путь`: папка и начало имени (в нижнем регистре).
struct FileScan {
    dir: PathBuf,
    prefix: String,
}

impl FileScan {
    /// Разобрать `@путь` относительно рабочей папки. Только строки, без диска.
    fn parse(path_part: &str, cwd: &Path) -> Self {
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
    fn run(&self) -> Vec<CommandSuggestion> {
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

impl App {
    /// Intercept dropdown-navigation keys while the autocomplete popup is
    /// open (and no modal owns the keyboard). Returns whether the key was
    /// consumed by the dropdown.
    pub(super) fn autocomplete_intercepts(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ac_visible = self.state.autocomplete.visible
            && !self.state.autocomplete.items.is_empty()
            && self.state.modal.is_none();
        if !ac_visible {
            return false;
        }
        // The dropdown only owns *bare* Tab/Up/Down/Enter. Any Ctrl/Alt-modified
        // key (notably Ctrl+Up/Down history recall) must fall through — otherwise
        // a menu opened by a recalled `/command` would trap the very keys used to
        // keep browsing history.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.step_autocomplete(1);
                true
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.step_autocomplete(-1);
                true
            }
            KeyCode::Enter if !self.state.focused_mut().generation.active => {
                self.accept_autocomplete();
                true
            }
            _ => false,
        }
    }

    pub(super) fn refresh_autocomplete(&mut self) {
        let buf = self.state.input.buffer.clone();

        // `@путь`: папку читает фоновая задача, результат придёт событием.
        if let Some(at_pos) = buf.rfind('@') {
            let after_at = &buf[at_pos + 1..];
            let path_part = after_at.split_whitespace().next().unwrap_or("");
            if !path_part.is_empty()
                && !self.state.focused_mut().generation.active
                && self.state.modal.is_none()
            {
                self.state.autocomplete.file_mode = true;
                if self.state.autocomplete.scanning {
                    // Скан уже идёт: после него прочтём заново по свежему вводу.
                    self.state.autocomplete.rescan = true;
                    return;
                }
                let scan = FileScan::parse(path_part, &self.state.workspace());
                let generation = next_scan_generation();
                self.state.autocomplete.wanted = generation;
                self.state.autocomplete.scanning = true;
                self.state.autocomplete.rescan = false;
                let event_tx = self.event_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let items = scan.run();
                    let _ = event_tx.send(AppEvent::AutocompleteReady { generation, items });
                });
                return;
            }
        }

        // Command autocomplete
        let query_main = buf
            .strip_prefix('/')
            .map(|rest| rest.split_whitespace().next().unwrap_or(""))
            .unwrap_or("");
        let active = buf.starts_with('/')
            && !self.state.focused_mut().generation.active
            && self.state.modal.is_none()
            && !buf[1..].contains(char::is_whitespace);
        // Ответ запоздавшего скана файлов теперь никому не нужен.
        self.state.autocomplete.wanted = 0;
        self.state.autocomplete.scanning = false;
        self.state.autocomplete.rescan = false;
        if !active {
            self.state.autocomplete.visible = false;
            self.state.autocomplete.items.clear();
            self.state.autocomplete.cursor.reset();
            return;
        }
        let items = self.commands.suggest(query_main);
        self.state.autocomplete.items = items;
        self.state.autocomplete.visible = !self.state.autocomplete.items.is_empty();
        self.state.autocomplete.cursor.reset();
        self.state.autocomplete.file_mode = false;
    }

    /// Скан папки закончился. Устаревший ответ (ввод успел измениться или
    /// закрыться) отбрасывается; если ввод менялся во время скана — читаем снова.
    pub(crate) fn on_autocomplete_ready(&mut self, generation: u64, items: Vec<CommandSuggestion>) {
        let autocomplete = &mut self.state.autocomplete;
        if generation != autocomplete.wanted {
            return;
        }
        autocomplete.scanning = false;
        autocomplete.visible = !items.is_empty();
        autocomplete.items = items;
        autocomplete.cursor.reset();
        autocomplete.file_mode = true;
        if std::mem::take(&mut autocomplete.rescan) {
            self.refresh_autocomplete();
        }
    }

    /// Автодополнение ходит по кругу — своя политика, общее окно.
    fn step_autocomplete(&mut self, delta: isize) {
        let n = self.state.autocomplete.items.len();
        if n == 0 {
            return;
        }
        let current = self.state.autocomplete.cursor.selected as isize;
        let next = (current + delta).rem_euclid(n as isize) as usize;
        self.state
            .autocomplete
            .cursor
            .move_to(next, n, AUTOCOMPLETE_VISIBLE);
    }

    fn accept_autocomplete(&mut self) {
        if !self.state.autocomplete.visible || self.state.autocomplete.items.is_empty() {
            return;
        }
        let idx = self
            .state
            .autocomplete
            .cursor
            .selected
            .min(self.state.autocomplete.items.len() - 1);
        let suggestion = &self.state.autocomplete.items[idx];

        if self.state.autocomplete.file_mode {
            let path = std::path::Path::new(&suggestion.usage);
            // Папку отличает косая черта в имени: `is_dir()` ходил бы на диск.
            if suggestion.name.ends_with('/') {
                let at_pos = self.state.input.buffer.rfind('@').unwrap_or(0);
                let before_at = self.state.input.buffer[..at_pos].to_string();
                let new_buf = format!("{}@{}/", before_at, suggestion.name.trim_end_matches('/'));
                self.state.input.buffer = new_buf;
                self.state.input.cursor = self.state.input.buffer.chars().count();
                self.state.input.selection_anchor = None;
                self.state.autocomplete = AutocompleteState::default();
            } else {
                let workspace = self.state.workspace();
                if let Ok(file) =
                    crate::app::attachments::resolve(&path.to_string_lossy(), &workspace)
                {
                    self.state.attached_files.push(file);
                }
                let at_pos = self.state.input.buffer.rfind('@').unwrap_or(0);
                let after_at = &self.state.input.buffer[at_pos + 1..];
                let path_text = after_at.split_whitespace().next().unwrap_or("");
                let before = self.state.input.buffer[..at_pos].to_string();
                let after = self.state.input.buffer[at_pos + 1 + path_text.len()..].to_string();
                let new_buf = format!("{}{}", before, after);
                self.state.input.buffer = new_buf;
                self.state.input.cursor = self.state.input.buffer.chars().count();
                self.state.input.selection_anchor = None;
                self.state.autocomplete = AutocompleteState::default();
                self.state.status_message =
                    crate::commands::defs::attach::attached_status(self.state.attached_files.len());
            }
        } else {
            let new_buf = format!("/{} ", suggestion.name);
            self.state.input.buffer = new_buf;
            self.state.input.cursor = self.state.input.buffer.chars().count();
            self.state.input.selection_anchor = None;
            self.state.autocomplete = AutocompleteState::default();
        }
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
        let scan = FileScan {
            dir: PathBuf::from("this-folder-does-not-exist-anywhere"),
            prefix: String::new(),
        };
        assert!(scan.run().is_empty());
    }

    #[test]
    fn scan_numbers_only_grow_and_never_hit_zero() {
        let first = next_scan_generation();
        let second = next_scan_generation();
        assert!(first > 0 && second > first);
    }
}
