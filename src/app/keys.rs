//! Key handling: the main key dispatcher, the MCP-view keys, and autocomplete.
//! These are `App` methods split out of `app/mod.rs`. Pure relocation.

use super::events::{self, GoalStage, Modal, View};
use super::{
    conversation, format_size, kill_foreground_child, App, AutocompleteState, AUTOCOMPLETE_VISIBLE,
};
use crate::commands::{CommandResult, CommandSuggestion};
use crate::error::AppResult;
use crate::provider::{ChatMessage, LLMProvider, Role};
use std::collections::HashSet;
use std::sync::Arc;

impl App {
    pub(crate) async fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Global Tab/Up/Down are intercepted by autocomplete when visible.
        let ac_visible = self.state.autocomplete.visible
            && !self.state.autocomplete.items.is_empty()
            && self.state.modal.is_none();
        if ac_visible {
            match key.code {
                KeyCode::Tab => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::BackTab => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + n - 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::Down => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::Up => {
                    let n = self.state.autocomplete.items.len();
                    self.state.autocomplete.selected =
                        (self.state.autocomplete.selected + n - 1) % n;
                    self.clamp_autocomplete_scroll();
                    return Ok(false);
                }
                KeyCode::Enter if !self.state.generation.active => {
                    self.accept_autocomplete();
                    return Ok(false);
                }
                _ => {}
            }
        }

        if let Some(modal) = self.state.modal.clone() {
            match modal {
                Modal::ToolApproval { tool_name, arguments, scroll_offset, always_allow } => {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            if always_allow {
                                self.state.approved_tools.insert(tool_name.clone());
                            }
                            if let Some(request) = self.state.pending_tool_approval.take() {
                                request.resolve(true).await;
                            }
                            self.state.modal = None;
                            self.state.generation.active = true;
                            self.state.status_message = format!("Running {}", tool_name);
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            if let Some(request) = self.state.pending_tool_approval.take() {
                                request.resolve(false).await;
                            }
                            self.state.modal = None;
                            self.state.generation.active = true;
                            self.state.status_message = format!("Denied {}", tool_name);
                        }
                        KeyCode::Char('a') | KeyCode::Char('A') => {
                            self.state.modal = Some(Modal::ToolApproval {
                                tool_name, arguments,
                                scroll_offset,
                                always_allow: !always_allow,
                            });
                        }
                        KeyCode::Up => {
                            let new_offset = scroll_offset.saturating_sub(3);
                            self.state.modal = Some(Modal::ToolApproval {
                                tool_name, arguments,
                                scroll_offset: new_offset,
                                always_allow,
                            });
                        }
                        KeyCode::Down => {
                            let arg_lines = arguments.lines().count();
                            let max_visible = 12usize;
                            let max_scroll = arg_lines.saturating_sub(max_visible);
                            let new_offset = (scroll_offset + 3).min(max_scroll);
                            self.state.modal = Some(Modal::ToolApproval {
                                tool_name, arguments,
                                scroll_offset: new_offset,
                                always_allow,
                            });
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                Modal::Picker(mut picker) => {
                    // Ctrl+A: toggle select all filtered items
                    if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let filtered_count = picker.items.len();
                        if filtered_count > 0 && picker.persistent_checked.len() >= filtered_count {
                            picker.persistent_checked.clear();
                        } else {
                            picker.persistent_checked = picker.items.iter().map(|item| item.value.clone()).collect();
                        }
                        picker.sync_checked();
                        self.state.modal = Some(Modal::Picker(picker));
                        return Ok(false);
                    }
                    match key.code {
                        KeyCode::Char(c) if !matches!(c, 'j' | 'k' | ' ') => {
                            let mut s = picker.search.clone();
                            s.push(c);
                            picker.update_search(s);
                            self.state.modal = Some(Modal::Picker(picker));
                            return Ok(false);
                        }
                        KeyCode::Backspace => {
                            let mut s = picker.search.clone();
                            s.pop();
                            picker.update_search(s);
                            self.state.modal = Some(Modal::Picker(picker));
                            return Ok(false);
                        }
                        _ => {}
                    }
                    let kind = picker.kind.clone();
                    let action = events::handle_picker_key(&mut picker, key.code);
                    match action {
                        events::PickerAction::Selected(indices) => {
                            match kind {
                                events::PickerKind::Whitelist => {
                                    let selected_names: HashSet<String> = indices
                                        .iter()
                                        .filter_map(|&i| picker.items.get(i))
                                        .map(|item| item.value.clone())
                                        .collect();
                                    if let Err(e) = crate::whitelist::save(&selected_names) {
                                        tracing::warn!("Failed to save whitelist: {e}");
                                    }
                                    self.state.approved_tools = selected_names;
                                    self.state.modal = None;
                                }
                                events::PickerKind::Skills => {
                                    let mut config = match crate::config::load() {
                                        Ok(c) => c,
                                        Err(_) => {
                                            self.state.modal = None;
                                            return Ok(false);
                                        }
                                    };
                                    let enabled: Vec<String> = indices
                                        .iter()
                                        .filter_map(|&i| picker.items.get(i))
                                        .map(|item| item.value.clone())
                                        .collect();
                                    config.skills.enabled = enabled.clone();
                                    if let Err(e) = crate::config::save(&config) {
                                        tracing::warn!("Failed to save skills config: {e}");
                                    }
                                    self.config.skills.enabled = enabled.clone();
                                    for skill in &mut self.skills {
                                        skill.enabled = enabled.contains(&skill.slug) || enabled.contains(&skill.name);
                                    }
                                    self.tools.update_skills(self.skills.clone());
                                    self.state.modal = None;
                                }
                                events::PickerKind::Chats => {
                                    let target = indices
                                        .first()
                                        .and_then(|&i| picker.items.get(i))
                                        .and_then(|item| item.value.parse::<u64>().ok())
                                        .map(conversation::ConversationId);
                                    self.state.modal = None;
                                    if let Some(target) = target {
                                        self.switch_to(target);
                                    }
                                }
                                events::PickerKind::Agents => {
                                    let target = indices
                                        .first()
                                        .and_then(|&i| picker.items.get(i))
                                        .and_then(|item| item.value.parse::<u64>().ok())
                                        .map(conversation::ConversationId);
                                    self.state.modal = None;
                                    if let Some(target) = target {
                                        self.stop_background(target);
                                    }
                                }
                                _ => {
                                    if let Some(idx) = indices.first() {
                                        if let Some(item) = picker.items.get(*idx) {
                                            let id = item.value.clone();
                                            self.state.modal = None;
                                            self.handle_load_session(&id).await?;
                                        }
                                    }
                                    self.state.modal = None;
                                }
                            }
                        }
                        events::PickerAction::Cancelled => {
                            self.state.modal = None;
                        }
                        events::PickerAction::None => {
                            self.state.modal = Some(Modal::Picker(picker));
                        }
                    }
                    return Ok(false);
                }
                Modal::Question(mut qs) => {
                    let result = events::handle_question_key(&mut qs, key.code);
                    if let Some(answer) = result {
                        if let Some(request) = self.state.pending_question.take() {
                            request.resolve(answer).await;
                        }
                        self.state.modal = None;
                        self.state.generation.active = true;
                        self.state.status_message = "Answer received".to_string();
                    } else {
                        self.state.modal = Some(Modal::Question(qs));
                        self.state.status_message = "Answering question...".to_string();
                    }
                    return Ok(false);
                }
            }
        }

        if self.state.view == View::Mcp {
            return self.handle_mcp_key(key).await;
        }

        match key.code {
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'c' | 'C') =>
            {
                if self.state.generation.active {
                    // Kill the running foreground child process first.
                    kill_foreground_child();
                    if let Some(handle) = self.agent_task.take() {
                        handle.abort();
                    }
                    let killed = self.shutdown_background_processes().await;
                    self.state.generation.active = false;
                    self.state.status_message = if killed > 0 {
                        format!("Cancelled; killed {killed} background process(es)")
                    } else {
                        "Cancelled".to_string()
                    };
                    self.state.needs_terminal_restore = true;
                    if self
                        .state
                        .messages
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                    {
                        self.state.messages.pop();
                    }
                    if self.state.goal.is_running() {
                        self.cancel_goal_cycle("⏹ Goal cycle cancelled. Use /goal to start a new one.");
                    }
                    return Ok(false);
                }
                let _ = self.shutdown_background_processes().await;
                return Ok(true);
            }
            KeyCode::Esc => {
                if self.state.generation.active {
                    // Kill the running foreground child process first.
                    kill_foreground_child();
                    // Cancel the current agent turn: abort the spawned task,
                    // reset is_generating so the user can type a new message.
                    if let Some(handle) = self.agent_task.take() {
                        handle.abort();
                    }
                    let killed = self.shutdown_background_processes().await;
                    self.state.generation.active = false;
                    self.state.status_message = if killed > 0 {
                        format!("Cancelled; killed {killed} background process(es)")
                    } else {
                        "Cancelled".to_string()
                    };
                    self.state.needs_terminal_restore = true;
                    if self
                        .state
                        .messages
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant && message.content.is_empty())
                    {
                        self.state.messages.pop();
                    }
                    if self.state.goal.is_running() {
                        self.cancel_goal_cycle("⏹ Goal cycle cancelled. Use /goal to start a new one.");
                    }
                } else if self.state.messages.is_empty() {
                    let _ = self.shutdown_background_processes().await;
                    return Ok(true);
                } else {
                    // Esc with no active turn clears the chat; if a goal was mid
                    // setup, clear that too so we don't orphan its state.
                    if self.state.goal.mode {
                        self.state.goal.deactivate();
                    }
                    self.state.messages.clear();
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
                self.state.messages.clear();
                self.state.scroll_offset = 0;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.select_all();
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.input.insert_newline();
            }
            KeyCode::Enter if !self.state.generation.active => {
                let buf = &self.state.input.buffer;
                let ends_with_backslash = buf
                    .chars()
                    .last()
                    .is_some_and(|c| c == '\\')
                    && self.state.input.cursor == buf.chars().count();
                if ends_with_backslash {
                    // Replace the trailing backslash with a newline (line continuation).
                    self.state.input.buffer.pop();
                    self.state.input.cursor -= 1;
                    self.state.input.insert_newline();
                } else {
                            let input = self.state.input.buffer.trim().to_string();
                            // Empty submission while defining a goal: nudge instead of silently ignoring it.
                            if input.is_empty()
                                && self.state.goal.mode
                                && matches!(self.state.goal.stage, GoalStage::Inactive | GoalStage::WaitForGoal)
                            {
                                let what = if matches!(self.state.goal.stage, GoalStage::Inactive) {
                                    "prompt"
                                } else {
                                    "goal"
                                };
                                self.state.messages.push(ChatMessage::system(&format!(
                                    "Your {what} is empty — type something before pressing Enter."
                                )));
                                self.state.input.buffer.clear();
                                self.state.input.cursor = 0;
                            }
                            if !input.is_empty() {
                                self.state.input.buffer.clear();
                                self.state.input.cursor = 0;
                                self.state.input.selection_anchor = None;
                                self.state.autocomplete = AutocompleteState::default();
                                self.state.input.history_index = None;
                                crate::session::append_history(&input);

                                // --- GOAL mode: intercept non-command input ---
                                if self.state.goal.mode && !input.starts_with('/') {
                                    match self.state.goal.stage {
                                        GoalStage::Inactive => {
                                            // First input in goal mode = the prompt
                                            self.state.goal.prompt = input.clone();
                                            self.state.goal.stage = GoalStage::WaitForGoal;
                                            self.state.messages.push(ChatMessage::user(&input));
                                            self.state.messages.push(ChatMessage::system(
                                                "🎯 Goal mode: now define your GOAL (what must be achieved)",
                                            ));
                                            return Ok(false);
                                        }
                                        GoalStage::WaitForGoal => {
                                            // Second input = the goal
                                            self.state.goal.text = input.clone();
                                            self.state.messages.push(ChatMessage::user(&format!(
                                                "GOAL: {}", input
                                            )));

                                            // Without a provider the worker can't run; advancing
                                            // the stage would wedge the cycle, so bail cleanly.
                                            if self.provider.is_none() {
                                                self.cancel_goal_cycle(
                                                    "No provider configured — cannot run the goal. Set your DeepSeek token, then /goal to retry.",
                                                );
                                                return Ok(false);
                                            }

                                            self.state.goal.stage = GoalStage::RunAgent1;
                                            self.state.goal.iteration = 1;

                                            // Build the agent 1 prompt: user's prompt + goal
                                            let agent1_prompt = format!(
                                                "{}\n\nIMPORTANT - GOAL to achieve: {}",
                                                self.state.goal.prompt, self.state.goal.text
                                            );
                                            self.send_to_agent(agent1_prompt).await?;
                                            return Ok(false);
                                        }
                                        GoalStage::RunAgent1 | GoalStage::RunEvaluator => {
                                            // Block input while goal cycle is active
                                            self.state.messages.push(ChatMessage::system(
                                                "Goal cycle in progress. Wait for it to finish or type /goal to cancel.",
                                            ));
                                            return Ok(false);
                                        }
                                        GoalStage::Done => {
                                            // After goal is done, regular input resumes
                                            self.state.goal.mode = false;
                                            self.state.goal.stage = GoalStage::Inactive;
                                        }
                                    }
                                }

                                if input.starts_with('/') {
                            let result =
                                self.commands.execute(&input, &mut self.state, &self.config);
                            match result {
                                CommandResult::Handled => {}
                                CommandResult::NeedsAgent(msg) => {
                                    let killed = self.cleanup_background_before_user_turn().await;
                                    if killed > 0 {
                                        self.state.messages.push(ChatMessage::system(&format!(
                                            "Cleaned {killed} ephemeral job(s) before the new turn."
                                        )));
                                    }
                                    self.state.messages.push(ChatMessage::user(&input));
                                    self.send_to_agent(msg).await?;
                                }
                                CommandResult::LoadSession(id) => {
                                    self.handle_load_session(&id).await?;
                                }
                                CommandResult::Quit => {
                                    let _ = self.shutdown_background_processes().await;
                                    return Ok(true);
                                }
                                CommandResult::ResetProvider => {
                                    if let Ok(config) = crate::config::load() {
                                        self.config = config;
                                    }
                                    self.provider = if self.config.provider.token.is_empty() {
                                        None
                                    } else {
                                        crate::provider::deepseek::DeepseekProvider::new(
                                            &self.config.provider,
                                            self.config.agent.rate_limit_ms,
                                            self.config.agent.max_retries,
                                        ).ok().map(|ds| Arc::new(ds) as Arc<dyn LLMProvider>)
                                    };
                                    self.state.current_session_id =
                                        crate::session::create_session_id();
                                }
                                CommandResult::TtlUpdate(ttl) => {
                                    self.config.mcp.cache_ttl = ttl;
                                    {
                                        let mut mcp = self.mcp.lock().await;
                                        mcp.set_cache_ttl(ttl);
                                    }
                                    self.state.messages.push(ChatMessage::system(
                                        &format!("MCP cache TTL set to {ttl}s"),
                                    ));
                                }
                                CommandResult::ReloadMcp => {
                                    self.state.messages.push(ChatMessage::system(
                                        "Reloading all MCP servers...",
                                    ));
                                    let mut mcp = self.mcp.lock().await;
                                    mcp.reload_all().await;
                                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                                    self.state.messages.push(ChatMessage::system(
                                        "MCP servers reloaded",
                                    ));
                                }
                                CommandResult::ShowTools => {
                                    let tools_text = self.build_tools_display().await;
                                    self.state.messages.push(ChatMessage::system(&tools_text));
                                }
                                CommandResult::Jobs(action) => {
                                    let jobs_text = match action {
                                        crate::commands::JobCommandAction::List => {
                                            self.build_background_processes_display().await
                                        }
                                        crate::commands::JobCommandAction::Kill(id) => {
                                            self.kill_background_job(id).await
                                        }
                                        crate::commands::JobCommandAction::Prune => {
                                            self.prune_background_jobs().await
                                        }
                                    };
                                    self.state.messages.push(ChatMessage::system(&jobs_text));
                                }
                                CommandResult::ShowSkills => {
                                    self.open_skill_picker().await;
                                }
                                CommandResult::ToggleSkill(name, enable) => {
                                    self.toggle_skill(&name, enable).await;
                                }
                                CommandResult::OpenWhitelist => {
                                    self.open_whitelist_picker().await;
                                }
                                CommandResult::Sidechat(question) => {
                                    self.spawn_sidechat(question).await?;
                                }
                                CommandResult::NewChat => {
                                    self.new_conversation();
                                }
                                CommandResult::OpenChats => {
                                    self.open_chats_picker().await;
                                }
                                CommandResult::SpawnAgent(prompt) => {
                                    let parent = self.state.focused_id;
                                    let label: String = prompt.chars().take(40).collect();
                                    self.spawn_sub_agent(parent, label, prompt).await?;
                                }
                                CommandResult::OpenAgents => {
                                    self.open_agents_picker().await;
                                }
                                CommandResult::Error(err) => {
                                    self.state.messages.push(ChatMessage::system(&err));
                                }
                            }
                        } else {
                            let killed = self.cleanup_background_before_user_turn().await;
                            if killed > 0 {
                                self.state.messages.push(ChatMessage::system(&format!(
                                    "Cleaned {killed} ephemeral job(s) before the new turn."
                                )));
                            }
                            let mut expanded = self.expand_file_mentions(&input);
                            if !self.state.attached_files.is_empty() {
                                let attach_header = if expanded.trim().is_empty() {
                                    String::new()
                                } else {
                                    format!("\n\n")
                                };
                                let attach_section: String = self.state.attached_files
                                    .iter()
                                    .filter_map(|f| {
                                        let content = std::fs::read_to_string(&f.path).ok()?;
                                        let header = format!("File: {} ({}):", f.display_name, crate::app::format_size(f.size));
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
                            self.state.messages.push(ChatMessage::user(&expanded));
                            self.send_to_agent(expanded).await?;
                        }
                    }
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
                self.state.input.move_home(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::End => {
                self.state.input.move_end(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::Up if !self.state.generation.active
                && self.state.input.buffer.chars().take(self.state.input.cursor).filter(|&c| c == '\n').count() == 0 =>
            {
                self.state.input.history_prev();
            }
            KeyCode::Down if !self.state.generation.active
                && self.state.input.buffer.chars().skip(self.state.input.cursor).filter(|&c| c == '\n').count() == 0 =>
            {
                self.state.input.history_next();
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

    pub(crate) async fn handle_mcp_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        if self.state.mcp_status.view.servers.is_empty() {
            let mcp = self.mcp.lock().await;
            self.state.mcp_status.view.servers = mcp.get_servers_info();
        }

        let details_open = self.state.mcp_status.view.details_server.is_some();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.mcp_status.view.active = false;
                self.state.mcp_status.view.details_server = None;
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') if !details_open => {
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.saturating_sub(1);
                self.clamp_mcp_scroll();
            }
            KeyCode::Down | KeyCode::Char('j') if !details_open => {
                let max = self.state.mcp_status.view.servers.len().saturating_sub(1);
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(max);
                self.state.mcp_status.view.selected += 1;
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(max);
                self.clamp_mcp_scroll();
            }
            KeyCode::Enter => {
                if details_open {
                    self.state.mcp_status.view.details_server = None;
                } else if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected) {
                    self.state.mcp_status.view.details_server = Some(info.name.clone());
                    self.state.mcp_status.view.scroll_offset = 0;
                }
            }
            KeyCode::Char(' ') if !details_open => {
                if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected).cloned() {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.toggle_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Toggle failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!(
                            "{} {}",
                            name,
                            if info.enabled { "disabled" } else { "enabled" },
                        );
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                }
            }
            KeyCode::Char('r') if !details_open => {
                if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected).cloned() {
                    let name = info.name.clone();
                    self.state.mcp_status.view.status_message = format!("Reconnecting {name}...");
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.reconnect_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Reconnect failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!("{name} reconnected");
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                }
            }
            KeyCode::Char('d') if !details_open => {
                if let Some(info) = self.state.mcp_status.view.servers.get(self.state.mcp_status.view.selected).cloned() {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.remove_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Remove failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!("{name} removed");
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                    self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(
                        self.state.mcp_status.view.servers.len().saturating_sub(1),
                    );
                }
            }
            KeyCode::Up | KeyCode::Char('k') if details_open => {
                self.state.mcp_status.view.scroll_offset = self.state.mcp_status.view.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if details_open => {
                self.state.mcp_status.view.scroll_offset += 1;
            }
            _ => {}
        }
        Ok(false)
    }

    pub(crate) fn clamp_mcp_scroll(&self) {
        // no-op for now, we manage scroll in the renderer
    }

    pub(crate) fn refresh_autocomplete(&mut self) {
        let buf = self.state.input.buffer.clone();

        // Check for @-triggered file path completion
        if let Some(at_pos) = buf.rfind('@') {
            let after_at = &buf[at_pos + 1..];
            let path_part = after_at.split_whitespace().next().unwrap_or("");
            if !path_part.is_empty() && !self.state.generation.active && self.state.modal.is_none() {
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
                if let Ok(read_dir) = std::fs::read_dir(
                    if path_part.contains('/') || path_part.contains('\\') {
                        if parent.is_absolute() {
                            parent.to_path_buf()
                        } else {
                            cwd.join(parent)
                        }
                    } else {
                        cwd.clone()
                    },
                ) {
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
            && !self.state.generation.active
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

    pub(crate) fn clamp_autocomplete_scroll(&mut self) {
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

    pub(crate) fn accept_autocomplete(&mut self) {
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
                    let is_image =
                        matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg");
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
