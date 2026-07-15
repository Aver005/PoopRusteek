//! `/serve` + `/server` effects: the API-server lifecycle as seen from the
//! TUI. Spawning/binding happens inside the server task (never on the
//! event loop); this file only flips the handle, persists config, and
//! reports. See `crate::server` for the server itself.

use crate::app::App;
use crate::commands::ServeAction;
use crate::server::{self, catalog};
use std::sync::atomic::Ordering;

impl App {
    pub(crate) fn apply_serve_action(&mut self, action: ServeAction) {
        match action {
            ServeAction::Status => {
                let text = self.server_status_text();
                self.state.push_system(&text);
            }
            ServeAction::Start => self.start_server("requested with /serve on"),
            ServeAction::Stop => match self.server.take() {
                Some(handle) => {
                    handle.request_shutdown();
                    self.state.push_system("API server stopping...");
                }
                None => self.state.push_system("API server is not running."),
            },
            ServeAction::SetPort(port) => {
                let previous = self.config.server.port;
                self.config.server.port = port;
                if let Err(error) = crate::config::save(&self.config) {
                    self.config.server.port = previous;
                    self.state
                        .push_system(&format!("Failed to save config: {error}"));
                    return;
                }
                if self.server.is_some() {
                    self.restart_server(&format!("port changed to {port}"));
                } else {
                    self.state.push_system(&format!(
                        "API server port set to {port} (saved). Start with /serve on."
                    ));
                }
            }
            ServeAction::SetApi(api) => {
                if !api.is_implemented() {
                    self.state.push_system(&format!(
                        "The '{}' dialect is reserved but not implemented yet — only 'openai' \
                         works today. The server keeps speaking openai.",
                        api.label()
                    ));
                    return;
                }
                let previous = self.config.server.api;
                self.config.server.api = api;
                if let Err(error) = crate::config::save(&self.config) {
                    self.config.server.api = previous;
                    self.state
                        .push_system(&format!("Failed to save config: {error}"));
                    return;
                }
                if self.server.is_some() {
                    self.restart_server(&format!("dialect changed to {}", api.label()));
                } else {
                    self.state.push_system(&format!(
                        "API dialect set to '{}' (saved). Start with /serve on.",
                        api.label()
                    ));
                }
            }
        }
    }

    /// Launch the server from the current config. Public because `--serve`
    /// (and its aliases) trigger it from `main` before the loop starts.
    pub fn start_server(&mut self, why: &str) {
        if self.server.is_some() {
            self.state
                .push_system("API server is already running — see /serve.");
            return;
        }
        let settings = server::ServerSettings::from_config(&self.config);
        if settings.deepseek.is_none() && settings.entries.is_empty() {
            self.state.push_system(
                "Note: no providers are configured — the API will expose no models. \
                 Set up DeepSeek or add one via /providers.",
            );
        }
        self.server_generation += 1;
        let handle = server::spawn(
            settings,
            std::sync::Arc::clone(&self.provider_models),
            self.server_generation,
            self.event_tx.clone(),
        );
        self.state.push_system(&format!(
            "API server starting on {}:{} ({} dialect) — {why}.",
            handle.host,
            handle.port,
            handle.api.label()
        ));
        self.server = Some(handle);
    }

    /// Signal the old task and immediately spawn the successor. The old
    /// listener may still be closing — the server task retries its bind
    /// briefly, which absorbs that race. Stale lifecycle events are
    /// filtered by generation.
    fn restart_server(&mut self, why: &str) {
        if let Some(handle) = self.server.take() {
            handle.request_shutdown();
        }
        self.start_server(why);
    }

    fn server_status_text(&self) -> String {
        let config = &self.config.server;
        let state = match &self.server {
            Some(handle) => match handle.bound_addr {
                Some(addr) => format!(
                    "running on http://{addr}/v1 (up {}, {} requests, {} errors)",
                    crate::app::format_duration_secs(handle.started_at.elapsed().as_secs()),
                    handle.stats.requests.load(Ordering::Relaxed),
                    handle.stats.errors.load(Ordering::Relaxed),
                ),
                None => "starting (binding the listener)...".to_string(),
            },
            None => format!("stopped (would bind {}:{})", config.host, config.port),
        };
        let models = catalog::list_model_ids(
            !self.config.provider.token.is_empty(),
            &self.config.providers,
            &self.provider_models.snapshot(),
        );
        // Fetched lists can run long (OpenRouter ≈ hundreds) — cap the chat
        // display; the full list is one `GET /v1/models` away.
        const SHOWN: usize = 24;
        let models = match models.len() {
            0 => "none — configure a provider first".to_string(),
            n if n > SHOWN => format!(
                "{} … +{} more (see /refetch-providers or GET /v1/models)",
                models[..SHOWN].join(", "),
                n - SHOWN,
            ),
            _ => models.join(", "),
        };
        let auth = match &config.api_key {
            Some(key) if !key.trim().is_empty() => "bearer token required",
            _ => "off (loopback bind is the guard)",
        };
        format!(
            "API server — one OpenAI-compatible endpoint over every configured provider.\n\
             Point any OpenAI-speaking tool at the base URL and pick a backend by model id \
             (`<entry>/<model>` reaches any model the upstream serves).\n\
             \n\
             State:   {state}\n\
             Dialect: {} (POST /v1/chat/completions, GET /v1/models; anthropic/gemini are planned)\n\
             Auth:    {auth}\n\
             Models:  {models}\n\
             \n\
             Subcommands:\n\
             \u{2022} /serve on|off   — start / stop\n\
             \u{2022} /server <port>  — set the port (saved; current: {}); restarts a running server\n\
             \u{2022} /serve api <d>  — wire dialect (openai today)\n\
             \n\
             Also: launch with --serve / --server / --api to start serving immediately.",
            config.api.label(),
            config.port,
        )
    }
}

impl App {
    /// `AppEvent::ServerStarted` — record the bound address and announce.
    /// Stale generations are ignored: a replaced server may report late.
    pub(crate) fn on_server_started(&mut self, generation: u64, addr: std::net::SocketAddr) {
        if let Some(handle) = &mut self.server
            && handle.generation == generation
        {
            handle.bound_addr = Some(addr);
            let message = format!(
                "API server listening on http://{addr}/v1 ({} dialect).",
                handle.api.label()
            );
            self.announce(message);
        }
    }

    /// `AppEvent::ServerFailed` — drop the handle (if it is still this
    /// generation's) and announce the failure.
    pub(crate) fn on_server_failed(&mut self, generation: u64, error: String) {
        if self
            .server
            .as_ref()
            .is_some_and(|handle| handle.generation == generation)
        {
            self.server = None;
        }
        self.announce(format!("API server failed to start: {error}"));
    }

    /// `AppEvent::ServerStopped` — only the *current* server's stop clears
    /// the handle; a replaced server (port-change restart) reports late.
    pub(crate) fn on_server_stopped(&mut self, generation: u64) {
        if self
            .server
            .as_ref()
            .is_some_and(|handle| handle.generation == generation)
        {
            self.server = None;
            self.announce("API server stopped.");
        }
    }
}
