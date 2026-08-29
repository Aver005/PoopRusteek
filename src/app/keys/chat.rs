//! Keys for the default chat view: global shortcuts (cancel, quit, panel,
//! chat switching), input editing, history recall, scroll, and Enter —
//! which routes through [`App::submit_input`] (goal-mode interception,
//! slash-command dispatch via `dispatch::apply_command_result`, or a plain
//! message turn).

use crate::app::{App, AutocompleteState};
use crate::error::AppResult;
use crate::provider::{AttachedFile, ChatMessage};

/// How `submit_input` left the turn: keep processing the key normally
/// (refresh autocomplete), swallow the key, or quit the app.
enum SubmitOutcome {
    Continue,
    Consumed,
    Quit,
}

/// Builds the fenced block sent to the model for each attached file, plus
/// the display names for the "📎 attached: …" summary. Never drops a file:
/// content that cannot be read as text becomes a placeholder note instead.
fn build_attachment_section(files: &[AttachedFile]) -> (String, Vec<String>) {
    let mut names = Vec::with_capacity(files.len());
    let blocks: Vec<String> = files
        .iter()
        .map(|f| {
            names.push(f.display_name.clone());
            let header = format!(
                "File: {} ({}):",
                f.display_name,
                crate::util::format_size(f.size)
            );
            // Not-UTF-8 and could-not-open are different facts: saying
            // "binary content" about a missing file misleads the model.
            let body = match std::fs::read_to_string(&f.path) {
                Ok(content) => content,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    let kind = if f.is_image {
                        "Image file"
                    } else {
                        "Binary file"
                    };
                    format!("{kind} — content not read as text.")
                }
                Err(e) => format!("Could not read file: {e}"),
            };
            format!("```\n{header}\n{body}\n```")
        })
        .collect();
    (blocks.join("\n"), names)
}

impl App {
    pub(super) async fn handle_chat_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'c' | 'C') =>
            {
                // `agent_task.is_some()` (not just generation.active) so a turn
                // wedged behind a lost approval can still be cancelled.
                if self.state.focused().generation.active
                    || self.state.focused().agent_task.is_some()
                {
                    self.cancel_focused_turn().await;
                    return Ok(false);
                }
                let _ = self.state.background.shutdown_all().await;
                return Ok(true);
            }
            KeyCode::Esc => {
                if self.state.focused().generation.active
                    || self.state.focused().agent_task.is_some()
                {
                    self.cancel_focused_turn().await;
                } else if self.state.focused_mut().messages.is_empty() {
                    let _ = self.state.background.shutdown_all().await;
                    return Ok(true);
                } else {
                    // Esc with no active turn clears the chat; if a goal was mid
                    // setup, clear that too so we don't orphan its state.
                    if self.state.goal.mode {
                        self.state.goal.deactivate();
                    }
                    self.state.focused_mut().messages.clear();
                    self.state.scroll_offset = 0;
                }
            }
            KeyCode::Tab => {
                self.cycle_focus(1);
            }
            KeyCode::BackTab => {
                self.cycle_focus(-1);
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.show_stats_panel = !self.state.show_stats_panel;
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.focused_mut().messages.clear();
                self.state.scroll_offset = 0;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.select_all();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.input.insert_newline();
            }
            KeyCode::Enter if !self.state.focused_mut().generation.active => {
                match self.submit_input().await? {
                    SubmitOutcome::Continue => {}
                    SubmitOutcome::Consumed => return Ok(false),
                    SubmitOutcome::Quit => return Ok(true),
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.insert_char(c);
            }
            KeyCode::Backspace => {
                self.state.input.backspace();
            }
            KeyCode::Delete => {
                self.state.input.delete_forward();
            }
            KeyCode::Left => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                self.state.input.move_left(shift, ctrl);
            }
            KeyCode::Right => {
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                self.state.input.move_right(shift, ctrl);
            }
            KeyCode::Home => {
                self.state
                    .input
                    .move_home(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::End => {
                self.state
                    .input
                    .move_end(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            // History recall lives on Ctrl+Up/Down everywhere so plain
            // Up/Down can always scroll the message window without the old
            // cursor-position disambiguation fighting the scroll.
            KeyCode::Up
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !self.state.focused_mut().generation.active =>
            {
                self.state.input.history_prev();
                // Browsing history is recall, not composition: a recalled
                // `/command` must not pop the menu (nor leave a stale one to
                // hijack the next Ctrl+Up). It reopens only once the user edits,
                // so skip the trailing refresh below.
                self.state.autocomplete = AutocompleteState::default();
                return Ok(false);
            }
            KeyCode::Down
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !self.state.focused_mut().generation.active =>
            {
                self.state.input.history_next();
                self.state.autocomplete = AutocompleteState::default();
                return Ok(false);
            }
            KeyCode::Up => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(1);
            }
            KeyCode::Down => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.state.scroll_offset = self.state.scroll_offset.saturating_sub(10);
            }
            _ => {}
        }
        self.refresh_autocomplete();
        Ok(false)
    }

    /// Handle plain Enter: backslash line-continuation, empty-goal nudges,
    /// then either goal-mode interception, a slash command, or a normal
    /// message turn (with `@file` expansion and attachments inlined for the
    /// model but not the chat view).
    async fn submit_input(&mut self) -> AppResult<SubmitOutcome> {
        let buf = &self.state.input.buffer;
        let ends_with_backslash = buf.chars().last().is_some_and(|c| c == '\\')
            && self.state.input.cursor == buf.chars().count();
        if ends_with_backslash {
            // Replace the trailing backslash with a newline (line continuation).
            self.state.input.buffer.pop();
            self.state.input.cursor -= 1;
            self.state.input.insert_newline();
            return Ok(SubmitOutcome::Continue);
        }

        // Expand any `[Pasted #N, L lines]` chips back to their real content so
        // the model (and the saved message) get the full pasted text.
        let input = self.state.input.expanded().trim().to_string();
        if input.is_empty() {
            // Empty while defining a goal gets a nudge instead of silence.
            self.maybe_nudge_empty_goal();
            return Ok(SubmitOutcome::Continue);
        }

        // Sending a message acknowledges any errors flagged since the last
        // one — clear the red marker (the text stays in errors.log).
        self.state.error_count = 0;
        self.state.last_error = None;

        self.state.input.clear_buffer();
        self.state.autocomplete = AutocompleteState::default();
        self.state.input.history_index = None;
        // Update the in-memory recall list synchronously (up-arrow must
        // see the new entry immediately), then queue the file write on the
        // persist worker — this used to be a blocking read-modify-write of
        // history.json right here on the event loop.
        crate::session::push_history_entry(&mut self.state.input.history, &input);
        self.persister
            .enqueue(crate::app::persist::PersistJob::WriteHistory(
                self.state.input.history.clone(),
            ));

        // GOAL mode intercepts non-command input — the whole state machine
        // lives in goal.rs; `false` means goal mode just ended and the
        // input proceeds as a normal turn.
        if self.state.goal.mode && !input.starts_with('/') && self.handle_goal_input(&input).await?
        {
            return Ok(SubmitOutcome::Consumed);
        }

        if input.starts_with('/') {
            let result = self.commands.execute(&input, &mut self.state, &self.config);
            if self.apply_command_result(result).await? {
                return Ok(SubmitOutcome::Quit);
            }
            return Ok(SubmitOutcome::Continue);
        }

        let killed = self.state.background.cleanup_before_user_turn().await;
        if killed > 0 {
            self.state.push_system(&format!(
                "Cleaned {killed} ephemeral job(s) before the new turn."
            ));
        }
        let mut expanded = self.expand_file_mentions(&input);
        let mut attached_names: Vec<String> = Vec::new();
        if !self.state.attached_files.is_empty() {
            let attach_header = if expanded.trim().is_empty() {
                String::new()
            } else {
                "\n\n".to_string()
            };
            let (attach_section, names) = build_attachment_section(&self.state.attached_files);
            attached_names = names;
            if !attach_section.is_empty() {
                expanded.push_str(&attach_header);
                expanded.push_str(&attach_section);
            }
            self.state.attached_files.clear();
        }
        // Attachment bodies go to the model, not the chat
        // view — rendering a 2 MB log inline (and re-scanning
        // it every frame) helps nobody.
        let message = if attached_names.is_empty() {
            ChatMessage::user(&expanded)
        } else {
            ChatMessage::user_with_display(
                &expanded,
                &format!("{}\n📎 attached: {}", input, attached_names.join(", ")),
            )
        };
        // Человек написал сам — цепочка автоматических побудок оборвана.
        self.reset_timer_wakes(self.state.conversations.focused_id());
        self.send_focused_turn(Some(message)).await?;
        Ok(SubmitOutcome::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_file(test_name: &str, name: &str, content: &str) -> String {
        let dir = std::env::temp_dir()
            .join("pooprusteek_attach_section_test")
            .join(test_name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn readable_text_file_is_inlined() {
        let path = scratch_file("readable", "notes.txt", "hello world");
        let files = [AttachedFile {
            display_name: "notes.txt".to_string(),
            path,
            size: 11,
            is_image: false,
        }];
        let (section, names) = build_attachment_section(&files);
        assert_eq!(names, vec!["notes.txt".to_string()]);
        assert!(section.contains("File: notes.txt"));
        assert!(section.contains("hello world"));
    }

    #[test]
    fn binary_image_gets_a_placeholder_not_dropped() {
        let dir = std::env::temp_dir()
            .join("pooprusteek_attach_section_test")
            .join("binary");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.png");
        std::fs::write(&path, [0x89u8, 0x50, 0x4E, 0x47, 0xFF, 0xFE, 0x00]).unwrap();
        let files = [AttachedFile {
            display_name: "photo.png".to_string(),
            path: path.to_string_lossy().into_owned(),
            size: 7,
            is_image: true,
        }];
        let (section, names) = build_attachment_section(&files);
        assert_eq!(names, vec!["photo.png".to_string()]);
        assert!(section.contains("File: photo.png"));
        assert!(section.contains("Image file — content not read as text."));
    }

    #[test]
    fn missing_image_reports_the_open_error_not_binary_content() {
        let files = [AttachedFile {
            display_name: "photo.png".to_string(),
            path: "does-not-exist.png".to_string(),
            size: 1024,
            is_image: true,
        }];
        let (section, names) = build_attachment_section(&files);
        assert_eq!(names, vec!["photo.png".to_string()]);
        assert!(section.contains("Could not read file:"));
        assert!(!section.contains("not read as text"));
    }

    #[test]
    fn missing_path_gets_a_placeholder_not_dropped() {
        let files = [AttachedFile {
            display_name: "gone.txt".to_string(),
            path: "does-not-exist.txt".to_string(),
            size: 0,
            is_image: false,
        }];
        let (section, names) = build_attachment_section(&files);
        assert_eq!(names, vec!["gone.txt".to_string()]);
        assert!(section.contains("File: gone.txt"));
        assert!(section.contains("Could not read file:"));
    }
}
