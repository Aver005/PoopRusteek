//! Autocomplete: dropdown navigation interception, suggestion refresh (both
//! `/command` and `@file` modes), and acceptance of the selected entry.

use crate::app::{AUTOCOMPLETE_VISIBLE, App, AutocompleteState, format_size};
use crate::commands::CommandSuggestion;

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
                let n = self.state.autocomplete.items.len();
                self.state.autocomplete.selected = (self.state.autocomplete.selected + 1) % n;
                self.clamp_autocomplete_scroll();
                true
            }
            KeyCode::BackTab | KeyCode::Up => {
                let n = self.state.autocomplete.items.len();
                self.state.autocomplete.selected = (self.state.autocomplete.selected + n - 1) % n;
                self.clamp_autocomplete_scroll();
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

        // Check for @-triggered file path completion
        if let Some(at_pos) = buf.rfind('@') {
            let after_at = &buf[at_pos + 1..];
            let path_part = after_at.split_whitespace().next().unwrap_or("");
            if !path_part.is_empty()
                && !self.state.focused_mut().generation.active
                && self.state.modal.is_none()
            {
                let cwd = std::env::current_dir().unwrap_or_default();
                let search_path = if path_part.contains('/') || path_part.contains('\\') {
                    std::path::Path::new(path_part).to_path_buf()
                } else {
                    cwd.join(path_part)
                };
                let parent = search_path.parent().unwrap_or(&cwd);
                let prefix = search_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                let mut items = Vec::new();
                if let Ok(read_dir) =
                    std::fs::read_dir(if path_part.contains('/') || path_part.contains('\\') {
                        if parent.is_absolute() {
                            parent.to_path_buf()
                        } else {
                            cwd.join(parent)
                        }
                    } else {
                        cwd.clone()
                    })
                {
                    for entry in read_dir.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.to_lowercase().starts_with(&prefix) {
                            let full_path = entry.path();
                            let display = full_path.to_string_lossy().to_string();
                            let is_dir = full_path.is_dir();
                            let suffix = if is_dir { "/" } else { "" };
                            let desc = if is_dir {
                                "dir".to_string()
                            } else {
                                let meta = full_path.metadata().ok();
                                let size = meta.map(|m| m.len()).unwrap_or(0);
                                format_size(size)
                            };
                            items.push(CommandSuggestion {
                                name: format!("{}{}", name, suffix),
                                description: desc,
                                usage: display.clone(),
                            });
                        }
                    }
                }
                items.sort_by(|a, b| {
                    let a_dir = a.usage.ends_with('/');
                    let b_dir = b.usage.ends_with('/');
                    a_dir.cmp(&b_dir).then(a.name.cmp(&b.name))
                });
                self.state.autocomplete.items = items;
                self.state.autocomplete.visible = !self.state.autocomplete.items.is_empty();
                self.state.autocomplete.selected = 0;
                self.state.autocomplete.scroll_offset = 0;
                self.state.autocomplete.file_mode = true;
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
        if !active {
            self.state.autocomplete.visible = false;
            self.state.autocomplete.items.clear();
            self.state.autocomplete.selected = 0;
            return;
        }
        let items = self.commands.suggest(query_main);
        self.state.autocomplete.items = items;
        self.state.autocomplete.visible = !self.state.autocomplete.items.is_empty();
        self.state.autocomplete.selected = 0;
        self.state.autocomplete.scroll_offset = 0;
        self.state.autocomplete.file_mode = false;
    }

    fn clamp_autocomplete_scroll(&mut self) {
        let n = self.state.autocomplete.items.len();
        if n <= AUTOCOMPLETE_VISIBLE {
            return;
        }
        let sel = self.state.autocomplete.selected;
        let off = &mut self.state.autocomplete.scroll_offset;
        if sel < *off {
            *off = sel;
        } else if sel >= *off + AUTOCOMPLETE_VISIBLE {
            *off = sel + 1 - AUTOCOMPLETE_VISIBLE;
        }
    }

    fn accept_autocomplete(&mut self) {
        if !self.state.autocomplete.visible || self.state.autocomplete.items.is_empty() {
            return;
        }
        let idx = self
            .state
            .autocomplete
            .selected
            .min(self.state.autocomplete.items.len() - 1);
        let suggestion = &self.state.autocomplete.items[idx];

        if self.state.autocomplete.file_mode {
            let path = std::path::Path::new(&suggestion.usage);
            if path.is_dir() {
                let at_pos = self.state.input.buffer.rfind('@').unwrap_or(0);
                let before_at = self.state.input.buffer[..at_pos].to_string();
                let new_buf = format!("{}@{}/", before_at, suggestion.name.trim_end_matches('/'));
                self.state.input.buffer = new_buf;
                self.state.input.cursor = self.state.input.buffer.chars().count();
                self.state.input.selection_anchor = None;
                self.state.autocomplete = AutocompleteState::default();
            } else {
                let resolved = if path.is_relative() {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    cwd.join(path)
                } else {
                    path.to_path_buf()
                };
                if resolved.is_file() {
                    let display_name = resolved
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();
                    let meta = resolved.metadata().ok();
                    let ext = resolved
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let is_image = matches!(
                        ext.as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg"
                    );
                    self.state
                        .attached_files
                        .push(crate::provider::AttachedFile {
                            display_name,
                            path: resolved.to_string_lossy().to_string(),
                            size: meta.map(|m| m.len()).unwrap_or(0),
                            is_image,
                        });
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
                    format!("{} files attached", self.state.attached_files.len());
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
