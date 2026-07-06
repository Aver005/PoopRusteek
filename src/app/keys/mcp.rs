//! Keys for the MCP surfaces: the `View::Mcp` management screen (list +
//! details), the `/mcp auth` picker, and every step of the `/mcp add`
//! modal (method choice, JSON paste, wizard). Slow MCP operations (toggle,
//! reconnect, add, OAuth) are spawned off the event loop and report back
//! via `AppEvent::McpOperationDone` / `McpOAuthResult`.

use super::apply_text_key;
use crate::app::App;
use crate::app::events::{self, Modal, View};
use crate::app::input::InputState;
use crate::app::mcp_add::{self, McpAddState, TransportChoice, WizardStep};
use crate::error::AppResult;
use crate::mcp::types::MCPServerConfig;
use crate::provider::ChatMessage;
use std::sync::Arc;

impl App {
    pub(super) async fn handle_mcp_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        if self.state.mcp_status.view.servers.is_empty() {
            let mcp = self.mcp.lock().await;
            self.state.mcp_status.view.servers = mcp.get_servers_info();
        }

        if self.state.mcp_status.view.auth_mode {
            return self.handle_mcp_auth_key(key).await;
        }

        let details_open = self.state.mcp_status.view.details_server.is_some();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.mcp_status.view.active = false;
                self.state.mcp_status.view.details_server = None;
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') if !details_open => {
                self.state.mcp_status.view.selected =
                    self.state.mcp_status.view.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if !details_open => {
                let max = self.state.mcp_status.view.servers.len().saturating_sub(1);
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(max);
                self.state.mcp_status.view.selected += 1;
                self.state.mcp_status.view.selected = self.state.mcp_status.view.selected.min(max);
            }
            KeyCode::Enter => {
                if details_open {
                    self.state.mcp_status.view.details_server = None;
                } else if let Some(info) = self
                    .state
                    .mcp_status
                    .view
                    .servers
                    .get(self.state.mcp_status.view.selected)
                {
                    self.state.mcp_status.view.details_server = Some(info.name.clone());
                    self.state.mcp_status.view.scroll_offset = 0;
                }
            }
            KeyCode::Char(' ') if !details_open => {
                if let Some(info) = self
                    .state
                    .mcp_status
                    .view
                    .servers
                    .get(self.state.mcp_status.view.selected)
                    .cloned()
                {
                    let name = info.name.clone();
                    let was_enabled = info.enabled;
                    self.state.mcp_status.view.status_message = format!(
                        "{} {name}...",
                        if was_enabled { "Disabling" } else { "Enabling" },
                    );
                    // Enabling spawns + initializes the server — off the loop.
                    let mcp = Arc::clone(&self.mcp);
                    let event_tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        let message = match mcp.lock().await.toggle_server(&name).await {
                            Err(e) => format!("Toggle failed: {e}"),
                            Ok(_) => format!(
                                "{name} {}",
                                if was_enabled { "disabled" } else { "enabled" },
                            ),
                        };
                        let _ = event_tx.send(events::AppEvent::McpOperationDone { message });
                    });
                }
            }
            KeyCode::Char('r') if !details_open => {
                if let Some(info) = self
                    .state
                    .mcp_status
                    .view
                    .servers
                    .get(self.state.mcp_status.view.selected)
                    .cloned()
                {
                    let name = info.name.clone();
                    self.state.mcp_status.view.status_message = format!("Reconnecting {name}...");
                    let mcp = Arc::clone(&self.mcp);
                    let event_tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        let message = match mcp.lock().await.reconnect_server(&name).await {
                            Err(e) => format!("Reconnect failed: {e}"),
                            Ok(_) => format!("{name} reconnected"),
                        };
                        let _ = event_tx.send(events::AppEvent::McpOperationDone { message });
                    });
                }
            }
            KeyCode::Char('d') if !details_open => {
                if let Some(info) = self
                    .state
                    .mcp_status
                    .view
                    .servers
                    .get(self.state.mcp_status.view.selected)
                    .cloned()
                {
                    let name = info.name.clone();
                    let mut mcp = self.mcp.lock().await;
                    if let Err(e) = mcp.remove_server(&name).await {
                        self.state.mcp_status.view.status_message = format!("Remove failed: {e}");
                    } else {
                        self.state.mcp_status.view.status_message = format!("{name} removed");
                    }
                    self.state.mcp_status.view.servers = mcp.get_servers_info();
                    self.state.mcp_status.view.selected = self
                        .state
                        .mcp_status
                        .view
                        .selected
                        .min(self.state.mcp_status.view.servers.len().saturating_sub(1));
                }
            }
            KeyCode::Up | KeyCode::Char('k') if details_open => {
                self.state.mcp_status.view.scroll_offset =
                    self.state.mcp_status.view.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if details_open => {
                self.state.mcp_status.view.scroll_offset += 1;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Key handling for `/mcp auth`'s picker: only servers with `needs_auth`
    /// are navigable (`McpViewState::visible_indices`), and Enter starts the
    /// OAuth flow for the selected one instead of opening the details panel.
    async fn handle_mcp_auth_key(&mut self, key: crossterm::event::KeyEvent) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        let visible = self.state.mcp_status.view.visible_indices();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.mcp_status.view.active = false;
                self.state.mcp_status.view.auth_mode = false;
                self.state.view = View::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.mcp_status.view.selected =
                    self.state.mcp_status.view.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = visible.len().saturating_sub(1);
                self.state.mcp_status.view.selected =
                    (self.state.mcp_status.view.selected + 1).min(max);
            }
            KeyCode::Enter => {
                let selected = self.state.mcp_status.view.selected;
                let Some(name) = visible
                    .get(selected)
                    .and_then(|&idx| self.state.mcp_status.view.servers.get(idx))
                    .map(|info| info.name.clone())
                else {
                    return Ok(false);
                };

                let context = { self.mcp.lock().await.oauth_context(&name) };
                let Some((config, hint)) = context else {
                    self.state.mcp_status.view.status_message =
                        format!("{name} no longer requires authorization");
                    return Ok(false);
                };

                let base_url = match config {
                    crate::mcp::types::MCPServerConfig::Http { url, .. } => url,
                    crate::mcp::types::MCPServerConfig::Sse { url, .. } => url,
                    crate::mcp::types::MCPServerConfig::Stdio { .. } => {
                        self.state.mcp_status.view.status_message =
                            "stdio servers don't use OAuth".to_string();
                        return Ok(false);
                    }
                };

                self.state.mcp_status.view.status_message =
                    format!("Opening browser for {name}...");
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let result = match crate::mcp::oauth::run_flow(&base_url, hint).await {
                        Ok(tokens) => crate::mcp::oauth_store::save(&name, &tokens)
                            .await
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = event_tx.send(events::AppEvent::McpOAuthResult {
                        server: name,
                        result,
                    });
                });
            }
            _ => {}
        }
        Ok(false)
    }

    /// Send `entries` to `MCPManager::add_new_server` one at a time, off the
    /// event loop (each add connects a fresh client, which can take a
    /// while). Reuses `AppEvent::McpOperationDone` for the result — same
    /// handling as reload/toggle/reconnect (refreshes the cached server
    /// list on the next poll).
    pub(super) fn spawn_mcp_add(&self, entries: Vec<(String, MCPServerConfig)>) {
        let mcp = Arc::clone(&self.mcp);
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let mut lines = Vec::with_capacity(entries.len());
            for (name, config) in entries {
                let outcome = mcp.lock().await.add_new_server(name.clone(), config).await;
                lines.push(match outcome {
                    Ok(status) => format!("{name}: added ({})", mcp_add::status_label(&status)),
                    Err(e) => format!("{name}: {e}"),
                });
            }
            let _ = event_tx.send(events::AppEvent::McpOperationDone {
                message: lines.join("\n"),
            });
        });
    }

    /// Key handling for every step of `/mcp add`'s modal: the method-choice
    /// screen, the JSON paste box, and the step-by-step wizard. Each
    /// text-entry step shares `apply_text_key` for the common editing keys
    /// and only adds its own Enter/Esc transition on top.
    pub(super) async fn handle_mcp_add_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        state: McpAddState,
    ) -> AppResult<bool> {
        use crossterm::event::KeyCode;

        match state {
            McpAddState::ChooseMethod { mut selected } => {
                match key.code {
                    KeyCode::Esc => {
                        self.state.modal = None;
                        return Ok(false);
                    }
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(1),
                    KeyCode::Enter => {
                        let next = if selected == 0 {
                            McpAddState::PasteJson {
                                input: InputState::default(),
                                error: None,
                            }
                        } else {
                            McpAddState::Wizard(Box::new(mcp_add::WizardState::new()))
                        };
                        self.state.modal = Some(Modal::McpAdd(next));
                        return Ok(false);
                    }
                    _ => {}
                }
                self.state.modal = Some(Modal::McpAdd(McpAddState::ChooseMethod { selected }));
            }
            McpAddState::PasteJson {
                mut input,
                mut error,
            } => {
                if !apply_text_key(&mut input, key, true) {
                    match key.code {
                        KeyCode::Esc => {
                            self.state.modal = Some(Modal::McpAdd(McpAddState::choose_method()));
                            return Ok(false);
                        }
                        KeyCode::Enter => match mcp_add::parse_pasted_json(&input.buffer) {
                            Ok(entries) => {
                                self.state.modal = None;
                                self.state
                                    .focused_mut()
                                    .messages
                                    .push(ChatMessage::ui_system(&format!(
                                        "Adding {} MCP server(s)...",
                                        entries.len()
                                    )));
                                self.spawn_mcp_add(entries);
                                return Ok(false);
                            }
                            Err(reason) => error = Some(reason),
                        },
                        _ => {}
                    }
                }
                self.state.modal = Some(Modal::McpAdd(McpAddState::PasteJson { input, error }));
            }
            McpAddState::Wizard(mut wiz) => {
                let handled_as_text = match wiz.step {
                    WizardStep::Name => apply_text_key(&mut wiz.name, key, false),
                    WizardStep::Primary => apply_text_key(&mut wiz.primary, key, false),
                    WizardStep::Extra => apply_text_key(&mut wiz.extra, key, true),
                    WizardStep::Transport | WizardStep::Confirm => false,
                };

                if !handled_as_text {
                    match key.code {
                        KeyCode::Esc => {
                            match wiz.step {
                                WizardStep::Name => {
                                    self.state.modal =
                                        Some(Modal::McpAdd(McpAddState::choose_method()));
                                    return Ok(false);
                                }
                                WizardStep::Transport => wiz.step = WizardStep::Name,
                                WizardStep::Primary => wiz.step = WizardStep::Transport,
                                WizardStep::Extra => wiz.step = WizardStep::Primary,
                                WizardStep::Confirm => wiz.step = WizardStep::Extra,
                            }
                            wiz.error = None;
                        }
                        KeyCode::Up | KeyCode::Char('k') if wiz.step == WizardStep::Transport => {
                            wiz.transport_selected = wiz.transport_selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') if wiz.step == WizardStep::Transport => {
                            wiz.transport_selected =
                                (wiz.transport_selected + 1).min(TransportChoice::ALL.len() - 1);
                        }
                        KeyCode::Enter => match wiz.step {
                            WizardStep::Name => {
                                let name = wiz.name.buffer.trim().to_string();
                                if name.is_empty() {
                                    wiz.error = Some("name can't be empty".to_string());
                                } else if self.mcp.lock().await.has_server(&name) {
                                    wiz.error = Some(format!("'{name}' already exists"));
                                } else {
                                    wiz.error = None;
                                    wiz.step = WizardStep::Transport;
                                }
                            }
                            WizardStep::Transport => {
                                wiz.transport = Some(TransportChoice::ALL[wiz.transport_selected]);
                                wiz.error = None;
                                wiz.step = WizardStep::Primary;
                            }
                            WizardStep::Primary => {
                                let result: Result<(), String> = match wiz.transport {
                                    Some(TransportChoice::Stdio) => {
                                        mcp_add::split_command_line(wiz.primary.buffer.trim())
                                            .map(|_| ())
                                    }
                                    _ => {
                                        let url = wiz.primary.buffer.trim();
                                        if url.is_empty() {
                                            Err("URL can't be empty".to_string())
                                        } else {
                                            reqwest::Url::parse(url)
                                                .map(|_| ())
                                                .map_err(|e| format!("invalid URL: {e}"))
                                        }
                                    }
                                };
                                match result {
                                    Ok(()) => {
                                        wiz.error = None;
                                        wiz.step = WizardStep::Extra;
                                    }
                                    Err(e) => wiz.error = Some(e),
                                }
                            }
                            WizardStep::Extra => {
                                let result = match wiz.transport {
                                    Some(TransportChoice::Stdio) => {
                                        mcp_add::parse_env_lines(&wiz.extra.buffer).map(|_| ())
                                    }
                                    _ => mcp_add::parse_header_lines(&wiz.extra.buffer).map(|_| ()),
                                };
                                match result {
                                    Ok(()) => {
                                        wiz.error = None;
                                        wiz.step = WizardStep::Confirm;
                                    }
                                    Err(e) => wiz.error = Some(e),
                                }
                            }
                            WizardStep::Confirm => match wiz.build_config() {
                                Ok(config) => {
                                    let name = wiz.name.buffer.trim().to_string();
                                    self.state.modal = None;
                                    self.state
                                        .focused_mut()
                                        .messages
                                        .push(ChatMessage::ui_system(&format!("Adding {name}...")));
                                    self.spawn_mcp_add(vec![(name, config)]);
                                    return Ok(false);
                                }
                                Err(e) => wiz.error = Some(e),
                            },
                        },
                        _ => {}
                    }
                }
                self.state.modal = Some(Modal::McpAdd(McpAddState::Wizard(wiz)));
            }
        }
        Ok(false)
    }
}
