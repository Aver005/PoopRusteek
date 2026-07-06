//! Headless proxy mode (`--proxy`, or any serve flag + `--uiless`): the
//! API server without the TUI. Stdout is ours here — no ratatui frame, no
//! `--acp` JSON-RPC — so the "UI" is a timestamped event/request log:
//! lifecycle, one line per HTTP request (method, path, status, duration,
//! resolved model), and provider-model refresh outcomes.
//!
//! Runs the same `spawn()` server as the TUI plus its own model-refresh
//! loop (the TUI drives refetch off its Tick; here a plain interval does
//! it). First Ctrl+C shuts down gracefully, second one hard-exits.

use super::{spawn, ServerSettings};
use crate::app::events::AppEvent;
use crate::config::Config;
use crate::error::AppResult;
use crate::provider::model_cache::ProviderModelCache;
use std::sync::Arc;

fn log_line(text: &str) {
    println!("[{}] {text}", chrono::Local::now().format("%H:%M:%S%.3f"));
}

pub async fn run(config: Config) -> AppResult<()> {
    let models = ProviderModelCache::load();
    let mut settings = ServerSettings::from_config(&config);
    settings.request_log = true;

    log_line(&format!(
        "pooprusteek proxy — {} dialect on {}:{}, auth {}",
        settings.api.label(),
        settings.host,
        settings.port,
        if settings.api_key.is_some() { "bearer" } else { "off" },
    ));
    if settings.deepseek.is_none() && settings.entries.is_empty() {
        log_line("warning: no providers configured — the API will expose no models");
    }

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = spawn(settings, Arc::clone(&models), 1, event_tx.clone());

    // Startup fetch (respects the persisted cache) + periodic refetch.
    {
        let models = Arc::clone(&models);
        let entries = config.providers.clone();
        let cache_ms = config.provider_models.cache_ms;
        let refetch_ms = config.provider_models.refetch_ms;
        let event_tx = event_tx.clone();
        if !entries.is_empty() {
            tokio::spawn(async move {
                let outcome = models.refresh(&entries, cache_ms, false).await;
                let _ = event_tx.send(AppEvent::ProviderModelsRefreshed {
                    summary: outcome.summary(),
                    failed: outcome.failed.len(),
                });
                if refetch_ms == 0 {
                    return;
                }
                // Floor of 1s: a sub-second refetch period would just melt
                // the upstreams for no informational gain.
                let period = std::time::Duration::from_millis(refetch_ms.max(1000));
                let mut interval = tokio::time::interval(period);
                interval.tick().await; // consume the immediate first tick
                loop {
                    interval.tick().await;
                    let outcome = models.refresh(&entries, cache_ms, true).await;
                    let _ = event_tx.send(AppEvent::ProviderModelsRefreshed {
                        summary: outcome.summary(),
                        failed: outcome.failed.len(),
                    });
                }
            });
        }
    }

    let mut shutting_down = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                if shutting_down {
                    log_line("second Ctrl+C — exiting immediately");
                    std::process::exit(130);
                }
                shutting_down = true;
                log_line("Ctrl+C — shutting down...");
                handle.request_shutdown();
            }
            event = event_rx.recv() => match event {
                None => break, // server task gone — nothing left to report
                Some(AppEvent::ServerStarted { addr, .. }) => {
                    log_line(&format!("listening on http://{addr}/v1"));
                }
                Some(AppEvent::ServerFailed { error, .. }) => {
                    log_line(&format!("failed to start: {error}"));
                    return Err(crate::error::AppError::Custom(error));
                }
                Some(AppEvent::ServerStopped { .. }) => {
                    log_line("server stopped");
                    break;
                }
                Some(AppEvent::ServerRequestLog { line }) => log_line(&line),
                Some(AppEvent::ProviderModelsRefreshed { summary, .. }) => log_line(&summary),
                // Nothing else runs in this process, but the enum is shared
                // with the TUI — ignore the rest.
                Some(_) => {}
            }
        }
    }
    Ok(())
}
