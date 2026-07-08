//! Keys for the default chat view: global shortcuts (cancel, quit, panel,
//! chat switching), input editing, history recall, scroll, and Enter —
//! which routes through [`App::submit_input`] (goal-mode interception,
//! slash-command dispatch via `dispatch::apply_command_result`, or a plain
//! message turn).

use crate::app::events::GoalStage;
use crate::app::{App, AutocompleteState};
use crate::error::AppResult;
use crate::provider::ChatMessage;

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
        // Empty submission while defining a goal: nudge instead of silently ignoring it.
        if input.is_empty()
            && self.state.goal.mode
            && matches!(
                self.state.goal.stage,
                GoalStage::Inactive | GoalStage::WaitForGoal
            )
        {
            let what = if matches!(self.state.goal.stage, GoalStage::Inactive) {
                "prompt"
            } else {
                "goal"
            };
            self.state.push_system(&format!(
                "Your {what} is empty — type something before pressing Enter."
            ));
            self.state.input.clear_buffer();
        }
        if input.is_empty() {
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

        // --- GOAL mode: intercept non-command input ---
        if self.state.goal.mode && !input.starts_with('/') {
            match self.state.goal.stage {
                GoalStage::Inactive => {
                    // First input in goal mode = the prompt.
                    // Echoes are ui_only: the model gets the
                    // combined agent-1 prompt once, below.
                    self.state.goal.prompt = input.clone();
                    self.state.goal.stage = GoalStage::WaitForGoal;
                    let mut echo = ChatMessage::user(&input);
                    echo.ui_only = true;
                    self.state.focused_mut().messages.push(echo);
                    self.state
                        .focused_mut()
                        .messages
                        .push(ChatMessage::ui_system(
                            "🎯 Goal mode: now define your GOAL (what must be achieved)",
                        ));
                    return Ok(SubmitOutcome::Consumed);
                }
                GoalStage::WaitForGoal => {
                    // Second input = the goal
                    self.state.goal.text = input.clone();
                    let mut echo = ChatMessage::user(&format!("GOAL: {}", input));
                    echo.ui_only = true;
                    self.state.focused_mut().messages.push(echo);

                    // Without a provider the worker can't run; advancing
                    // the stage would wedge the cycle, so bail cleanly.
                    if self.state.focused().provider.is_none() {
                        self.cancel_goal_cycle(
                            "No provider configured — cannot run the goal. Set your DeepSeek token, then /goal to retry.",
                        );
                        return Ok(SubmitOutcome::Consumed);
                    }

                    self.state.goal.stage = GoalStage::RunAgent1;
                    self.state.goal.iteration = 1;

                    // Build the agent 1 prompt: user's prompt + goal
                    let agent1_prompt = format!(
                        "{}\n\nIMPORTANT - GOAL to achieve: {}",
                        self.state.goal.prompt, self.state.goal.text
                    );
                    let message = ChatMessage::user_with_display(
                        &agent1_prompt,
                        "[Goal cycle started — attempt 1]",
                    );
                    self.send_focused_turn(Some(message)).await?;
                    return Ok(SubmitOutcome::Consumed);
                }
                GoalStage::RunAgent1 | GoalStage::RunEvaluator => {
                    // Block input while goal cycle is active
                    self.state.focused_mut().messages.push(ChatMessage::ui_system(
                        "Goal cycle in progress. Wait for it to finish or type /goal to cancel.",
                    ));
                    return Ok(SubmitOutcome::Consumed);
                }
                GoalStage::Done => {
                    // After goal is done, regular input resumes
                    self.state.goal.mode = false;
                    self.state.goal.stage = GoalStage::Inactive;
                }
            }
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
            let attach_section: String = self
                .state
                .attached_files
                .iter()
                .filter_map(|f| {
                    let content = std::fs::read_to_string(&f.path).ok()?;
                    let header = format!(
                        "File: {} ({}):",
                        f.display_name,
                        crate::app::format_size(f.size)
                    );
                    attached_names.push(f.display_name.clone());
                    Some(format!("```\n{}\n{}\n```", header, content))
                })
                .collect::<Vec<_>>()
                .join("\n");
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
        self.send_focused_turn(Some(message)).await?;
        Ok(SubmitOutcome::Continue)
    }
}
