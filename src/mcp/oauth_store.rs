//! Encrypted persistence for MCP OAuth tokens: one OS-keyring entry per
//! server (Windows Credential Manager / macOS Keychain / Linux Secret
//! Service via the `keyring` crate's default `v1` feature). Deliberately
//! never touches `mcp.json` — tokens don't pass through any plaintext file
//! at all, unlike the rest of the config store (the DeepSeek token in
//! `config.toml` is still plaintext; this module doesn't fix that, but sets
//! the precedent for how secrets should be kept).

use super::oauth::{self, TokenSet};
use crate::error::{AppError, AppResult};
use std::collections::HashMap;

const SERVICE: &str = "pooprusteek-mcp";

/// Register a default credential store with `keyring-core` ourselves,
/// bypassing `keyring` 4.1.3's own auto-init in `v1::Entry::new`.
///
/// That auto-init is broken: it gates calling its internal
/// `set_credential_store()` behind
/// `SET_CREDENTIAL_STORE.compare_exchange(false, true, ..) == Ok(true)`, but
/// `compare_exchange`'s `Ok` variant carries the *previous* value — a
/// successful CAS starting from `false` always returns `Ok(false)`, never
/// `Ok(true)` — so that branch can never run. No store ever gets
/// registered, and every `Entry::new` fails with
/// `Error::NoDefaultStore` ("No default store has been set..."), forever,
/// since the same `AtomicBool` still flips to `true` as a side effect and
/// blocks any retry. Verified directly against `keyring-4.1.3/src/v1.rs`.
///
/// Worked around here with the same one-time-init idea, done correctly via
/// `std::sync::Once`, using the exact backend crates `keyring`'s own `v1`
/// feature already compiles in per target (see `Cargo.toml`'s
/// `[target...]` tables) — so this adds no new dependency edges, just names
/// them directly so `keyring_core::set_default_store` can be called.
fn ensure_default_store() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        #[cfg(target_os = "windows")]
        if let Ok(store) = windows_native_keyring_store::Store::new() {
            keyring_core::set_default_store(store);
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Ok(store) = apple_native_keyring_store::keychain::Store::new() {
            keyring_core::set_default_store(store);
        }
        #[cfg(all(
            unix,
            not(any(target_os = "macos", target_os = "ios", target_os = "android"))
        ))]
        if let Ok(store) = zbus_secret_service_keyring_store::Store::new() {
            keyring_core::set_default_store(store);
        }
    });
}

fn entry(server_name: &str) -> AppResult<keyring::Entry> {
    ensure_default_store();
    keyring::Entry::new(SERVICE, server_name)
        .map_err(|e| AppError::Mcp(format!("Keyring unavailable for '{server_name}': {e}")))
}

/// Persist `tokens` for `server_name`. `keyring`'s API is synchronous, so
/// this runs on `spawn_blocking` per the repo's "never block the event
/// loop" rule.
pub async fn save(server_name: &str, tokens: &TokenSet) -> AppResult<()> {
    let server_name = server_name.to_string();
    let tokens = tokens.clone();
    tokio::task::spawn_blocking(move || {
        let json = serde_json::to_string(&tokens)?;
        entry(&server_name)?
            .set_password(&json)
            .map_err(|e| AppError::Mcp(format!("Failed to save token for '{server_name}': {e}")))
    })
    .await?
}

/// Load stored tokens for `server_name`. `None` on *any* failure — missing
/// entry, unavailable backend (e.g. headless Linux with no Secret Service
/// running), or a corrupt blob. Must stay best-effort: plenty of servers
/// never had (or need) a token, and a missing/broken one should look
/// identical to "not authorized yet" rather than a hard error.
pub async fn load(server_name: &str) -> Option<TokenSet> {
    let server_name = server_name.to_string();
    tokio::task::spawn_blocking(move || {
        let entry = entry(&server_name).ok()?;
        let json = entry.get_password().ok()?;
        serde_json::from_str::<TokenSet>(&json).ok()
    })
    .await
    .ok()
    .flatten()
}

/// Best-effort: load `server_name`'s stored token, refreshing it first if
/// it's within 60s of expiring and a refresh token is available (re-saving
/// on success), then merge an `Authorization: Bearer` header into `headers`.
/// Never errors — any failure (no stored token, refresh failed, keyring
/// unavailable) just returns `headers` unchanged, which reproduces the
/// existing 401 → `AuthRequired` path and is self-healing once the user
/// re-authorizes via `/mcp auth`.
pub async fn with_bearer_header(
    server_name: &str,
    mut headers: HashMap<String, String>,
) -> HashMap<String, String> {
    let Some(mut tokens) = load(server_name).await else {
        return headers;
    };

    if tokens.is_expiring_soon() {
        match oauth::refresh(&tokens).await {
            Ok(fresh) => {
                let _ = save(server_name, &fresh).await;
                tokens = fresh;
            }
            Err(e) => {
                tracing::debug!(
                    "MCP '{server_name}' OAuth token refresh failed (best-effort): {e}"
                );
            }
        }
    }

    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {}", tokens.access_token),
    );
    headers
}
