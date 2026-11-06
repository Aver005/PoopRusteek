//! Autocomplete: dropdown navigation interception, suggestion refresh (both
//! `/command` and `@file` modes), and acceptance of the selected entry.

use crate::app::events::AppEvent;
use crate::app::file_scan::{FileScan, Launch, next_scan_generation};
use crate::app::{AUTOCOMPLETE_VISIBLE, App, AutocompleteState};
use crate::commands::CommandSuggestion;

/// Какой `@путь` стоит в буфере: его скан и нужен, если ввод сейчас можно
/// дополнять. `None` — файловых подсказок не нужно.
fn wanted_scan(buffer: &str, workspace: &std::path::Path) -> Option<FileScan> {
    let at_pos = buffer.rfind('@')?;
    let path_part = buffer[at_pos + 1..].split_whitespace().next().unwrap_or("");
    (!path_part.is_empty()).then(|| FileScan::parse(path_part, workspace))
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
        if !self.state.focused_mut().generation.active
            && self.state.modal.is_none()
            && let Some(scan) = wanted_scan(&buf, &self.state.workspace())
        {
            self.state.autocomplete.file_mode = true;
            if let Some(launch) = self.state.file_scans.want(scan, next_scan_generation()) {
                self.spawn_file_scan(launch);
            }
            return;
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
        // Ответ запоздавшего скана файлов теперь никому не нужен; идущий скан
        // доработает сам, но показан не будет.
        self.state.file_scans.leave();
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

    /// Запустить скан папки на блокирующем потоке; ответ придёт событием.
    fn spawn_file_scan(&self, (generation, scan): Launch) {
        let event_tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let items = scan.run();
            let _ = event_tx.send(AppEvent::AutocompleteReady { generation, items });
        });
    }

    /// Скан папки закончился. Устаревший ответ (ввод успел измениться, закрыться
    /// или уйти из `@`) отбрасывается; освободившийся слот берёт самый свежий
    /// отложенный запрос.
    pub(crate) fn on_autocomplete_ready(&mut self, generation: u64, items: Vec<CommandSuggestion>) {
        let (show, next) = self
            .state
            .file_scans
            .finish(generation, next_scan_generation);
        // Меню могли закрыть Esc или Enter, не сказав сканам: сверяем с буфером.
        let still_wanted = wanted_scan(&self.state.input.buffer, &self.state.workspace()).is_some()
            && !self.state.focused().generation.active
            && self.state.modal.is_none();
        if show && still_wanted {
            let autocomplete = &mut self.state.autocomplete;
            autocomplete.visible = !items.is_empty();
            autocomplete.items = items;
            autocomplete.cursor.reset();
            autocomplete.file_mode = true;
        }
        if let Some(launch) = next {
            self.spawn_file_scan(launch);
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
