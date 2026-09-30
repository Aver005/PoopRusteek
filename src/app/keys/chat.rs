//! Keys for the default chat view: global shortcuts (cancel, quit, panel,
//! chat switching), input editing, history recall, scroll, and Enter —
//! which routes through [`App::submit_input`] (goal-mode interception,
//! slash-command dispatch via `dispatch::apply_command_result`, or a plain
//! message turn).

use crate::app::{App, AutocompleteState};
use crate::error::AppResult;

/// How `submit_input` left the turn: keep processing the key normally
/// (refresh autocomplete), swallow the key, or quit the app.
enum SubmitOutcome {
    Continue,
    Consumed,
    Quit,
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
        // Одни файлы без текста — тоже сообщение, но не пустая цель.
        if input.is_empty() && (self.state.attached_files.is_empty() || self.state.goal.mode) {
            // Empty while defining a goal gets a nudge instead of silence.
            self.maybe_nudge_empty_goal();
            return Ok(SubmitOutcome::Continue);
        }

        // Sending a message acknowledges any errors flagged since the last
        // one — clear the red marker (the text stays in errors.log).
        self.state.error_count = 0;
        self.state.last_error = None;

        let chip_files = self.state.input.chip_files();
        // Команде и цели файл из чипа нужен путём: `/attach x [📎 a.pdf]` так и работает.
        let with_paths = self.state.input.with_chip_paths(&input);
        self.state.input.clear_buffer();
        self.state.autocomplete = AutocompleteState::default();
        self.state.input.end_recall();
        // Update the in-memory recall list synchronously (up-arrow must
        // see the new entry immediately), then queue the file write on the
        // persist worker — this used to be a blocking read-modify-write of
        // history.json right here on the event loop.
        if !input.is_empty() {
            crate::session::push_history_entry(&mut self.state.input.history, &input);
            self.persister
                .enqueue(crate::app::persist::PersistJob::WriteHistory(
                    self.state.input.history.clone(),
                ));
        }

        // GOAL mode intercepts non-command input — the whole state machine
        // lives in goal.rs; `false` means goal mode just ended and the
        // input proceeds as a normal turn.
        if self.state.goal.mode
            && !input.starts_with('/')
            && self.handle_goal_input(&with_paths).await?
        {
            return Ok(SubmitOutcome::Consumed);
        }

        if input.starts_with('/') {
            let result = self
                .commands
                .execute(&with_paths, &mut self.state, &self.config);
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
        let expanded = self.expand_file_mentions(&input);
        let provider = self.state.focused().provider.clone();
        let accepts = |path: &std::path::Path| {
            provider
                .as_ref()
                .is_some_and(|provider| provider.accepts_attachment(path))
        };
        // Прикреплённые через `/attach` и `@` в тексте не видны — их назовёт
        // сводка; чипы и `@файл` видны и так.
        let mut named = std::mem::take(&mut self.state.attached_files);
        named.retain(|f| !chip_files.iter().any(|c| c.path == f.path));
        let mut files = named.clone();
        let mentioned = crate::app::attachments::binary_mentions(&input, &self.state.workspace());
        for file in chip_files.into_iter().chain(mentioned) {
            crate::app::attachments::push_unique(&mut files, file);
        }
        // Содержимое файлов уходит модели, а не в чат: рисовать 2 МБ лога
        // в ленте (и пересчитывать их каждый кадр) никому не нужно.
        let message =
            crate::app::attachments::user_message(&expanded, &input, &files, &named, &accepts);
        // Человек написал сам — цепочка автоматических побудок оборвана.
        self.reset_timer_wakes(self.state.conversations.focused_id());
        self.send_focused_turn(Some(message)).await?;
        Ok(SubmitOutcome::Continue)
    }
}
