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

fn entry(server_name: &str) -> AppResult<keyring::Entry> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Круг записать–прочитать–удалить в настоящем хранилище ОС.
    #[test]
    #[ignore = "нужно настоящее хранилище ОС и живая сессия пользователя"]
    fn keyring_round_trip() {
        let name = format!("pooprusteek-selftest-{}", std::process::id());
        let entry = entry(&name).expect("запись keyring должна открываться");

        entry.set_password("self-test").expect("запись");
        assert_eq!(entry.get_password().expect("чтение"), "self-test");

        entry.delete_credential().expect("удаление");
        assert!(
            entry.get_password().is_err(),
            "после удаления записи быть не должно"
        );
    }
}
