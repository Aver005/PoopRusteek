//! The `CommandResult` interpreter: slash commands return an *intent*
//! (`CommandResult` variant) and this is the one place those intents turn
//! into effects on the `App` — provider resets, pickers, MCP operations,
//! sub-agent spawns. Keeping it out of the key-decoding path means a new
//! command effect touches exactly this match and nothing else.

use crate::app::events::{self, Modal, View};
use crate::app::mcp_add::{self, McpAddState};
use crate::app::App;
use crate::commands::CommandResult;
use crate::error::AppResult;
use crate::provider::ChatMessage;
use std::sync::Arc;

impl App {
    /// Apply a command's result to the app. Returns `true` when the command
    /// asks the whole app to quit.
    pub(super) async fn apply_command_result(&mut self, result: CommandResult) -> AppResult<bool> {
        match result {
            CommandResult::Handled => {}
            CommandResult::LoadSession(id) => {
                self.handle_load_session(&id).await?;
            }
            CommandResult::Quit => {
                let _ = self.state.background.shutdown_all().await;
                return Ok(true);
            }
            CommandResult::ResetProvider => {
                if let Ok(config) = crate::config::load() {
                    self.config = config;
                }
                self.rebuild_provider();
            }
            CommandResult::TtlUpdate(ttl) => {
                self.config.mcp.cache_ttl = ttl;
                {
                    let mut mcp = self.mcp.lock().await;
                    mcp.set_cache_ttl(ttl);
                }
                self.state.push_system(&format!("MCP cache TTL set to {ttl}s"));
            }
            CommandResult::ReloadMcp => {
                self.state.focused_mut().messages.push(ChatMessage::ui_system(
                    "Reloading all MCP servers...",
                ));
                // Off the event loop: reconnecting every
                // server can take seconds per server.
                let mcp = Arc::clone(&self.mcp);
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    mcp.lock().await.reload_all().await;
                    let _ = event_tx.send(events::AppEvent::McpOperationDone {
                        message: "MCP servers reloaded".to_string(),
                    });
                });
            }
            CommandResult::OpenMcpAuth => {
                self.state.view = View::Mcp;
                if self.state.mcp_status.view.servers.is_empty() {
                    let mcp = self.mcp.lock().await;
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                }
                self.state.mcp_status.view.active = true;
                self.state.mcp_status.view.auth_mode = true;
                self.state.mcp_status.view.details_server = None;
                self.state.mcp_status.view.selected = 0;
                self.state.mcp_status.view.scroll_offset = 0;
            }
            CommandResult::OpenMcpAdd(args) => {
                match args {
                    None => {
                        self.state.modal = Some(Modal::McpAdd(McpAddState::choose_method()));
                    }
                    Some(raw) => {
                        match mcp_add::parse_quick_add(&raw) {
                            Ok(entries) => {
                                self.state.focused_mut().messages.push(ChatMessage::ui_system(
                                    &format!("Adding {} MCP server(s)...", entries.len()),
                                ));
                                self.spawn_mcp_add(entries);
                            }
                            Err(reason) => {
                                self.state.focused_mut().messages.push(ChatMessage::ui_system(
                                    &format!("Couldn't parse \"/mcp add {raw}\" as a quick config ({reason}) \u{2014} pick a method:"),
                                ));
                                self.state.modal = Some(Modal::McpAdd(McpAddState::choose_method()));
                            }
                        }
                    }
                }
            }
            CommandResult::ShowTools => {
                let tools_text = self.build_tools_display().await;
                self.state.push_system(&tools_text);
            }
            CommandResult::Jobs(action) => {
                let jobs_text = match action {
                    crate::commands::JobCommandAction::List => {
                        self.build_background_processes_display().await
                    }
                    crate::commands::JobCommandAction::Kill(id) => {
                        self.state.background.kill_job(id).await
                    }
                    crate::commands::JobCommandAction::Prune => {
                        self.state.background.prune_jobs().await
                    }
                };
                self.state.push_system(&jobs_text);
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
                let parent = self.state.conversations.focused_id();
                let label: String = prompt.chars().take(40).collect();
                self.spawn_sub_agent(parent, label, prompt).await?;
            }
            CommandResult::OpenAgents => {
                self.open_agents_picker().await;
            }
            CommandResult::OpenDeleteSessions { scope, session_id } => {
                self.open_delete_sessions(scope, session_id);
            }
            CommandResult::OpenConfirm(action) => {
                self.open_confirm(action);
            }
            CommandResult::Error(err) => {
                self.state.push_system(&err);
            }
            CommandResult::OpenProviders => {
                self.state.view = View::Providers;
                self.state.providers_view.selected = 0;
                self.state.providers_view.status_message.clear();
            }
            CommandResult::OpenProviderAdd(args) => match args {
                None => {
                    self.state.modal = Some(Modal::ProviderAdd(Box::new(
                        crate::app::providers::ProviderAddState::new(),
                    )));
                }
                Some(raw) => {
                    match crate::app::providers::parse_quick_add(&raw, &self.config) {
                        Ok(entry) => {
                            let name = entry.name.clone();
                            self.config.providers.push(entry);
                            match crate::config::save(&self.config) {
                                Ok(()) => {
                                    self.state.push_system(&format!(
                                        "Provider '{name}' added. Open /providers and press Enter on it to activate."
                                    ));
                                }
                                Err(error) => {
                                    self.config.providers.pop();
                                    self.state.push_system(&format!(
                                        "Failed to save config: {error}"
                                    ));
                                }
                            }
                        }
                        Err(reason) => {
                            self.state.focused_mut().messages.push(ChatMessage::ui_system(
                                &format!("Couldn't parse \"/providers add {raw}\" ({reason}) \u{2014} opening the wizard:"),
                            ));
                            self.state.modal = Some(Modal::ProviderAdd(Box::new(
                                crate::app::providers::ProviderAddState::new(),
                            )));
                        }
                    }
                }
            },
        }
        Ok(false)
    }
}
