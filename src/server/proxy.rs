//! Headless proxy mode (`--proxy`, or any serve flag + `--uiless`): the
//! API server without the TUI. Stdout is ours here — no ratatui frame, no
//! `--acp` JSON-RPC — so the "UI" is a timestamped event/request log:
//! lifecycle, one line per HTTP request (method, path, status, duration,
//! resolved model), and provider-model refresh outcomes.
//!
//! Runs the same `spawn()` server as the TUI plus its own model-refresh
//! loop (the TUI drives refetch off its Tick; here a plain interval does
//! it). First Ctrl+C shuts down gracefully, second one hard-exits.

use super::{GatewayCell, ServerSettings, spawn};
use crate::app::events::AppEvent;
use crate::config::Config;
use crate::error::AppResult;
use crate::gateway::handle::{GatewayEvent, GatewayHandle};
use crate::gateway::pending::PendingGateway;
use crate::provider::model_cache::ProviderModelCache;
use std::sync::Arc;

fn log_line(text: &str) {
    println!("[{}] {text}", chrono::Local::now().format("%H:%M:%S%.3f"));
}

pub async fn run(config: Config) -> AppResult<()> {
    let models = ProviderModelCache::load();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    // Шлюз собирается так же, как в приложении: вне цикла событий, запросы ждут готовности.
    let (gateway, mut pending) = crate::app::start_gateway(&config, &event_tx);
    models.set_gateway(gateway.clone());
    let gateway_on = gateway.is_enabled();
    let mut settings = ServerSettings::from_config(&config);
    settings.request_log = true;
    settings.gateway = GatewayCell::new(gateway);

    log_line(&format!(
        "pooprusteek proxy — {} dialect on {}:{}, auth {}",
        settings.api.label(),
        settings.host,
        settings.port,
        if settings.api_key.is_some() {
            "bearer"
        } else {
            "off"
        },
    ));
    // Заметки загрузки конфига, в том числе «проектный конфиг ждёт `pooprusteek trust`».
    for notice in &config.migration_notices {
        log_line(notice);
    }
    if !gateway_on && settings.deepseek.is_none() && settings.entries.is_empty() {
        log_line("warning: no providers configured — the API will expose no models");
    }

    let handle = spawn(settings, Arc::clone(&models), 1, event_tx.clone());

    // Startup fetch (respects the persisted cache) + periodic refetch.
    {
        let models = Arc::clone(&models);
        let entries = config.providers.clone();
        let cache_ms = config.provider_models.cache_ms;
        let refetch_ms = config.provider_models.refetch_ms;
        let event_tx = event_tx.clone();
        if !entries.is_empty() || gateway_on {
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
                Some(AppEvent::GatewayBuilt(built)) => {
                    on_gateway_built(
                        built.0,
                        pending.take(),
                        &models,
                        &handle,
                        &event_tx,
                        (&config.providers, config.provider_models.cache_ms),
                    );
                }
                Some(AppEvent::ProviderModelsRefreshed { summary, .. }) => log_line(&summary),
                // Nothing else runs in this process, but the enum is shared
                // with the TUI — ignore the rest.
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// Движок собран: ждущие запросы продолжаются, сервер и кэш моделей переходят на него.
fn on_gateway_built(
    built: Result<GatewayHandle, String>,
    pending: Option<Arc<PendingGateway>>,
    models: &Arc<ProviderModelCache>,
    handle: &super::ServerHandle,
    event_tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    refresh: (&[crate::config::ProviderEntry], u64),
) {
    let Some(pending) = pending else { return };
    match built {
        Ok(gateway) => {
            pending.fulfil(Arc::clone(&gateway));
            models.set_gateway(Arc::clone(&gateway));
            handle.gateway.set(Arc::clone(&gateway));
            log_line("gateway ready");
            // Адаптеры собраны: каталог моделей аккаунтов шлюза наполняется сразу.
            let (models, entries, cache_ms, tx) = (
                Arc::clone(models),
                refresh.0.to_vec(),
                refresh.1,
                event_tx.clone(),
            );
            tokio::spawn(async move {
                let outcome = models.refresh(&entries, cache_ms, false).await;
                let _ = tx.send(AppEvent::ProviderModelsRefreshed {
                    summary: outcome.summary(),
                    failed: outcome.failed.len(),
                });
            });
            forward_fallbacks(&gateway, event_tx.clone());
        }
        Err(reason) => {
            pending.fail(reason.clone());
            log_line(&format!("the gateway failed to start: {reason}"));
        }
    }
}

/// Переходы на запасной аккаунт попадают в журнал запросов.
fn forward_fallbacks(
    gateway: &GatewayHandle,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    if !gateway.is_enabled() {
        return;
    }
    let mut events = gateway.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(GatewayEvent::FallbackTaken {
                    from, to, class, ..
                }) => {
                    let line = format!("fallback {from} → {to} ({})", class.code());
                    let _ = event_tx.send(AppEvent::ServerRequestLog { line });
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::testkit::{Script, engine, start, upstream};

    /// Прокси собирает шлюз так же, как приложение: пока он строится, запрос получает 503, потом сервер переходит на готовый.
    #[tokio::test]
    async fn the_proxy_moves_the_server_onto_the_gateway_once_it_is_built() {
        unsafe { std::env::set_var("POOPRUSTEEK_PROXY_TEST_KEY", "proxy-test-key-0000") };
        let up = upstream(vec![(
            "/a/",
            vec![Script::Stream {
                chunks: vec!["hello"],
                usage: None,
                headers: Vec::new(),
            }],
        )])
        .await;
        let text = format!(
            "[gateway]\nschema = 1\nenabled = true\n\n[[accounts]]\nid = \"a\"\nprovider = \"openai\"\nbase_url = \"http://127.0.0.1:{}/a/v1\"\nsecret = \"env:POOPRUSTEEK_PROXY_TEST_KEY\"\nmodel = \"m\"\n",
            up.port
        );
        let parsed = crate::config::parse(&text).unwrap();
        let pending = PendingGateway::new(crate::gateway::engine::gateway_config(&parsed));
        let building: GatewayHandle =
            Arc::new(crate::gateway::pending::PendingHandle(Arc::clone(&pending)));
        let stand = start(building, None).await;
        let ask = || async {
            reqwest::Client::new()
                .post(format!("{}/v1/chat/completions", stand.base))
                .json(&serde_json::json!({"model": "a/m", "messages": [{"role": "user", "content": "hi"}]}))
                .send()
                .await
                .unwrap()
                .status()
        };
        assert_eq!(ask().await, 503);

        let models = ProviderModelCache::empty_for_tests();
        let (event_tx, _events) = tokio::sync::mpsc::unbounded_channel();
        on_gateway_built(
            Ok(engine(&text, Vec::new())),
            Some(pending),
            &models,
            &stand.handle,
            &event_tx,
            (&[], 0),
        );
        assert_eq!(ask().await, 200);
    }
}
