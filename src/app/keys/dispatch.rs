//! The `CommandResult` interpreter: slash commands return an *intent*
//! (`CommandResult` variant) and this is the one place those intents turn
//! into effects on the `App` — provider resets, pickers, MCP operations,
//! sub-agent spawns. Keeping it out of the key-decoding path means a new
//! command effect touches exactly this match and nothing else.

use crate::app::App;
use crate::app::events::{self, Modal, View};
use crate::app::mcp_add::{self, McpAddState};
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
                self.state
                    .push_system(&format!("MCP cache TTL set to {ttl}s"));
            }
            CommandResult::ReloadMcp => {
                self.state
                    .focused_mut()
                    .messages
                    .push(ChatMessage::ui_system("Reloading all MCP servers..."));
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
                self.state.mcp_status.view.details_scroll = 0;
            }
            CommandResult::OpenMcpAdd(args) => match args {
                None => {
                    self.state.modal = Some(Modal::McpAdd(McpAddState::choose_method()));
                }
                Some(raw) => match mcp_add::parse_quick_add(&raw) {
                    Ok(entries) => {
                        self.state
                            .focused_mut()
                            .messages
                            .push(ChatMessage::ui_system(&format!(
                                "Adding {} MCP server(s)...",
                                entries.len()
                            )));
                        self.spawn_mcp_add(entries);
                    }
                    Err(reason) => {
                        self.state.focused_mut().messages.push(ChatMessage::ui_system(
                                    &format!("Couldn't parse \"/mcp add {raw}\" as a quick config ({reason}) \u{2014} pick a method:"),
                                ));
                        self.state.modal = Some(Modal::McpAdd(McpAddState::choose_method()));
                    }
                },
            },
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
            CommandResult::Compact(mode) => {
                self.spawn_compaction(mode);
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
            CommandResult::OpenModels => {
                self.request_models(None);
            }
            CommandResult::SwitchModel(id) => {
                self.request_models(Some(id));
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
                            self.config.providers.push(entry.clone());
                            match crate::config::save_or_message(&self.config) {
                                Ok(()) => {
                                    // Pull its model list right away so the
                                    // API catalog knows the new backend.
                                    self.fetch_models_for_new_entry(&entry);
                                    self.state.push_system(&format!(
                                        "Provider '{name}' added. Open /providers and press Enter on it to activate."
                                    ));
                                }
                                Err(message) => {
                                    self.config.providers.pop();
                                    self.state.push_system(&message);
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
            CommandResult::Rag(action) => self.apply_rag_action(action),
            CommandResult::OpenThemes => {
                self.state.view = View::Themes;
                self.state.themes = crate::app::themes::ThemesViewState::open(&self.config);
            }
            CommandResult::OpenThemeWizard => {
                self.state.view = View::Themes;
                self.state.themes = crate::app::themes::ThemesViewState::open(&self.config);
                self.state.themes.wizard = Some(crate::app::themes::ThemeWizardState::new());
            }
            CommandResult::SetTheme(name) => {
                let known = crate::tui::theme::PRESETS
                    .iter()
                    .any(|preset| preset.name == name)
                    || self
                        .config
                        .ui
                        .custom_themes
                        .iter()
                        .any(|theme| theme.name == name);
                if known {
                    self.apply_theme(&name);
                    // apply_theme reports into the themes view; echo the
                    // outcome into the chat the command was typed in.
                    let message = std::mem::take(&mut self.state.themes.status_message);
                    self.state.push_system(&format!("Theme: {message}."));
                } else {
                    let available: Vec<&str> = crate::tui::theme::PRESETS
                        .iter()
                        .map(|preset| preset.name)
                        .chain(
                            self.config
                                .ui
                                .custom_themes
                                .iter()
                                .map(|theme| theme.name.as_str()),
                        )
                        .collect();
                    self.state.push_system(&format!(
                        "Unknown theme '{name}'. Available: {} — or /themes to browse with live preview.",
                        available.join(", "),
                    ));
                }
            }
            CommandResult::Serve(action) => {
                self.apply_serve_action(action);
            }
            CommandResult::ProviderModels(action) => {
                self.apply_provider_models_action(action);
            }
            CommandResult::Update(action) => {
                self.apply_update_action(action);
            }
            CommandResult::SearchHistory(query) => {
                self.state.view = View::Search;
                self.state.search = crate::app::search::SearchViewState::fresh(&query);
                if !query.is_empty() {
                    self.spawn_history_search(query);
                }
            }
        }
        Ok(false)
    }

    /// `/rag` effects: status display, persisted on/off switch, full
    /// reload. Everything heavy (model load, embedding) happens inside the
    /// service on blocking threads — these arms only flip flags and spawn.
    fn apply_rag_action(&mut self, action: crate::commands::RagAction) {
        use crate::commands::RagAction;
        match action {
            RagAction::Status => {
                let text = self.rag_status_text();
                self.state.push_system(&text);
            }
            RagAction::SetEnabled(on) => {
                self.config.semantic.enabled = on;
                if let Err(message) = crate::config::save_or_message(&self.config) {
                    self.state.push_system(&message);
                    return;
                }
                self.semantic.set_enabled(on);
                if on {
                    if !self.semantic.is_ready() {
                        self.semantic
                            .spawn_init(self.skills.clone(), self.event_tx.clone());
                        self.state.push_system(
                            "RAG enabled — initializing in the background (first run downloads the ~120 MB embedding model). Hints start once it's ready.",
                        );
                    } else {
                        self.state.push_system("RAG enabled.");
                    }
                } else {
                    self.state.push_system(
                        "RAG disabled: no more hints, full MCP schemas return to the system prompt from the next turn. Re-enable with /rag on.",
                    );
                }
            }
            RagAction::Reload => {
                if !self.semantic.is_enabled() {
                    self.state
                        .push_system("RAG is disabled — run /rag on first.");
                    return;
                }
                if !self
                    .semantic
                    .reload(self.skills.clone(), self.event_tx.clone())
                {
                    self.state.push_system(
                        "RAG is already initializing — wait for it to finish before reloading.",
                    );
                    return;
                }
                // Also refresh the raw MCP tool list so the rebuilt index
                // can't resurrect tools from servers that have gone away.
                self.spawn_mcp_semantic_refresh();
                self.state.push_system(
                    "RAG reloading: verifying the model (missing files are re-downloaded) and re-embedding skills + MCP tools in the background.",
                );
            }
            RagAction::LimitStatus => {
                let text = self.rag_limit_status_text();
                self.state.push_system(&text);
            }
            RagAction::SetLimit(limit) => {
                self.config.semantic.rag_limit = limit;
                if let Err(message) = crate::config::save_or_message(&self.config) {
                    self.state.push_system(&message);
                    return;
                }
                let resolved = self.semantic.set_embed_batch(limit);
                let effect = match resolved {
                    Some(n) => format!("batch {n}"),
                    None => "no cap (fastembed default)".to_string(),
                };
                let when = if self.semantic.is_ready() {
                    "applied to the running embedder"
                } else {
                    "takes effect once the embedder loads"
                };
                self.state.push_system(&format!(
                    "RAG embedder limit → {limit} = {effect} ({when})."
                ));
            }
        }
    }

    /// `/update` + `/autoupdate` effects: the network check and binary swap
    /// run in a spawned task (`app::spawn_update_task`) — these arms only
    /// flip the persisted flag and report.
    fn apply_update_action(&mut self, action: crate::commands::UpdateAction) {
        use crate::commands::UpdateAction;
        match action {
            UpdateAction::Run => {
                // A debug/`cargo run` binary always mismatches the released
                // hash, so an unconfirmed /update would silently swap the dev
                // build for the release. Make that an explicit choice; a
                // release build updates straight away.
                if cfg!(debug_assertions) {
                    self.open_confirm(crate::app::events::ConfirmAction::Update);
                } else {
                    self.start_manual_update();
                }
            }
            UpdateAction::AutoStatus => {
                let state = if self.config.update.auto { "on" } else { "off" };
                self.state.push_system(&format!(
                    "Auto-update on startup: {state}.\n\
                     When on, every launch compares the binary's SHA-256 with the `latest` \
                     release in the background and installs on mismatch (applied on the next start).\n\
                     \u{2022} /autoupdate on|off — toggle\n\
                     \u{2022} /update — check and install right now"
                ));
            }
            UpdateAction::SetAuto(on) => {
                self.config.update.auto = on;
                if let Err(message) = crate::config::save_or_message(&self.config) {
                    self.state.push_system(&message);
                    return;
                }
                self.state.push_system(if on {
                    "Auto-update enabled — every startup checks the `latest` release in the background."
                } else {
                    "Auto-update disabled. /update still works manually."
                });
            }
        }
    }

    /// Kick off a manual (`/update`) check + install off the event loop.
    /// Shared by the release-build direct path and the dev-build confirm arm
    /// (`ConfirmAction::Update`).
    pub(super) fn start_manual_update(&mut self) {
        self.state
            .push_system("Checking for updates against the `latest` release\u{2026}");
        crate::app::spawn_update_task(
            self.event_tx.clone(),
            Arc::clone(&self.update_in_flight),
            false,
        );
    }

    /// `/rag-limit` status: the configured mode, the batch it resolves to,
    /// and the RAM that sizing is based on.
    fn rag_limit_status_text(&self) -> String {
        let mode = self.config.semantic.rag_limit;
        let ram = match crate::util::total_ram_bytes() {
            Some(b) => format!("{:.1} GB", b as f64 / 1e9),
            None => "unknown".to_string(),
        };
        let effective = match self.semantic.resolved_batch() {
            Some(n) => format!("batch {n}"),
            None => "no cap (fastembed default 256)".to_string(),
        };
        format!(
            "RAG embedder batch limit (ONNX memory guard)\n\
             • mode: {mode}\n\
             • detected RAM: {ram}\n\
             • effective: {effective}\n\n\
             Peak embed memory ≈ batch × ~13 MB. `auto` sizes it from RAM; \
             `off` removes the cap; a number pins it. Change: /rag-limit <N|auto|off>"
        )
    }

    fn rag_status_text(&self) -> String {
        // `status()` declines instead of waiting when an indexing pass
        // holds the index lock — this runs on the event loop, and waiting
        // out a corpus rebuild here used to freeze the whole UI.
        let Some(status) = self.semantic.status() else {
            return "RAG — indexing in progress (embedding skills / MCP tools / session history); \
                    detailed status is briefly unavailable. Run /rag again in a few seconds."
                .to_string();
        };
        let state = match (status.enabled, status.ready, status.init_running) {
            (false, ..) => "disabled",
            (true, true, _) => "enabled, ready",
            (true, false, true) => "enabled, initializing...",
            (true, false, false) => "enabled, not initialized (init failed? try /rag reload)",
        };
        let model = if status.model_on_disk {
            format!(
                "on disk ({})",
                crate::config::Config::data_dir().join("models").display()
            )
        } else {
            "not downloaded yet (~120 MB, fetched on first init)".to_string()
        };
        format!(
            "RAG — local semantic matching over skills and MCP tools.\n\
             Every prompt is embedded locally (multilingual-e5-small, ONNX; no network after the one-time model download) and matched against both catalogs — dense cosine + stemmed keyword TF-IDF, fused with RRF. Top hits ride along with the turn as an advisory hint; with many MCP tools connected, full schemas leave the system prompt and are fetched via the tool_search tool instead.\n\
             \n\
             State:   {state}\n\
             Model:   {model}\n\
             Indexed: {} skills, {} MCP tools ({} known; deferred schemas: {}), {} history chunks from {} sessions\n\
             Config:  top_k={}, min_dense_score={:.2}, mcp_schemas={:?}\n\
             \n\
             Subcommands:\n\
             \u{2022} /rag on      — enable (downloads the model on first use)\n\
             \u{2022} /rag off     — disable completely; full MCP schemas return to the system prompt\n\
             \u{2022} /rag reload  — reinitialize: verify/re-download the model, re-embed skills, MCP tools and session history\n\
             \n\
             Related: /search <query> — search past conversations; the agent has tool_search and history_search.",
            status.skills_indexed,
            status.mcp_indexed,
            status.mcp_known,
            if status.deferred { "on" } else { "off" },
            status.history_chunks,
            status.history_sessions,
            self.config.semantic.top_k,
            self.config.semantic.min_dense_score,
            self.config.semantic.mcp_schemas,
        )
    }
}
