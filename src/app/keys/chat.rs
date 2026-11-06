//! Keys for the default chat view: global shortcuts (cancel, quit, panel,
//! chat switching), input editing, history recall, scroll, and Enter —
//! which routes through [`App::submit_input`] (goal-mode interception,
//! slash-command dispatch via `dispatch::apply_command_result`, or a plain
//! message turn).

use crate::app::conversation::QueuedMessage;
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
                } else if self.state.autocomplete.visible {
                    self.state.autocomplete = AutocompleteState::default();
                } else if self.state.goal.is_setup_pending() {
                    // Настройка цели — не история: её можно отменить, текст поля цел.
                    self.state.goal.deactivate();
                    self.state.status_message = "GOAL setup cancelled".to_string();
                } else if !self.state.focused().queued.is_empty() {
                    // Очередь, что висит без хода (упавший ход), возвращается в поле.
                    let count = self.state.restore_queued_to_input();
                    self.state.status_message =
                        format!("{count} queued message(s) returned to the input");
                } else {
                    // В простое Esc ничего не стирает и не выходит: чат чистит
                    // только `/clear`, выход — Ctrl+C.
                    self.state.input.selection_anchor = None;
                }
            }
            KeyCode::Tab => {
                self.cycle_focus(1);
            }
            KeyCode::BackTab => {
                self.cycle_focus(-1);
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let visible = self.state.stats_panel_visible(self.state.terminal_cols);
                self.state.show_stats_panel = Some(!visible);
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Не стирает историю: перерисовка и прыжок вниз. Стирает `/clear`.
                self.state.scroll_offset = 0;
                self.state.needs_terminal_restore = true;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.select_all();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.input.insert_newline();
            }
            KeyCode::Enter => match self.submit_input().await? {
                SubmitOutcome::Continue => {}
                SubmitOutcome::Consumed => return Ok(false),
                SubmitOutcome::Quit => return Ok(true),
            },
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
    /// model but not the chat view). Пока ход идёт, обычный текст встаёт в
    /// очередь и уходит, когда ход кончится.
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
        // Пустое поле при стоящей очереди — «отправь следующее».
        if input.is_empty()
            && self.state.attached_files.is_empty()
            && !self.state.focused().generation.active
            && !self.state.focused().queued.is_empty()
        {
            self.state.focused_mut().queue_paused = false;
            let focused = self.state.conversations.focused_id();
            self.send_next_queued(focused).await;
            return Ok(SubmitOutcome::Continue);
        }
        // Одни файлы без текста — тоже сообщение, но не пустая цель.
        if input.is_empty() && (self.state.attached_files.is_empty() || self.state.goal.mode) {
            // Empty while defining a goal gets a nudge instead of silence.
            self.maybe_nudge_empty_goal();
            return Ok(SubmitOutcome::Continue);
        }

        // Команды и ввод цели во время хода не ставим в очередь: у них свой
        // порядок, и они меняют то, что ход читает. Текст остаётся в поле.
        let turn_running = self.state.focused().generation.active;
        if turn_running && (input.starts_with('/') || self.state.goal.mode) {
            self.state.status_message =
                "Commands wait for the turn to end — Esc cancels it".to_string();
            return Ok(SubmitOutcome::Continue);
        }

        let chips = self.state.input.present_chips();
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

        if turn_running {
            // Файлы `/attach` уходят с этим сообщением, а не с тем, что
            // окажется в полосе вложений к концу хода.
            let named = std::mem::take(&mut self.state.attached_files);
            self.state.focused_mut().queued.push_back(QueuedMessage {
                text: input,
                chips,
                named,
            });
            self.state.status_message = format!(
                "Queued ({}) — sent when the turn ends",
                self.state.focused().queued.len()
            );
            return Ok(SubmitOutcome::Continue);
        }

        // Sending a message acknowledges any errors flagged since the last
        // one — clear the red marker (the text stays in errors.log). Постановка
        // в очередь ничего не отправляет и метку не гасит.
        self.state.error_count = 0;
        self.state.last_error = None;

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

        let focused = self.state.conversations.focused_id();
        let named = std::mem::take(&mut self.state.attached_files);
        self.send_user_text(focused, input, chips, named).await?;
        Ok(SubmitOutcome::Continue)
    }
}
