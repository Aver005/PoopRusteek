//! OAuth 2.1 authorization-code + PKCE flow for MCP HTTP/SSE servers that
//! answer with HTTP 401 (RFC 9728 protected-resource discovery, RFC 8414
//! authorization-server metadata, RFC 7591 dynamic client registration).
//!
//! Started by the `/mcp auth` (`/mcp oauth`) key handler once a server is in
//! `MCPServerStatus::AuthRequired`. This module only produces a [`TokenSet`]
//! — it never touches `MCPManager` state. The caller is responsible for
//! persisting the result (`oauth_store::save`) and reconnecting the server
//! through the existing `MCPManager::reconnect_server` path.

use crate::error::{AppError, AppResult};
use base64::Engine as _;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Everything obtained from one completed authorization-code exchange.
/// Serialized as-is by `oauth_store` into the OS keyring.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` means the server didn't send `expires_in` — treat
    /// as long-lived rather than guessing a lifetime.
    pub expires_at: Option<i64>,
    pub token_endpoint: String,
    pub client_id: String,
}

impl TokenSet {
    /// True once fewer than 60s remain (or already past `expires_at`).
    /// `None` (unknown expiry) is treated as never-expiring.
    pub fn is_expiring_soon(&self) -> bool {
        let Some(exp) = self.expires_at else { return false };
        let now = unix_now();
        exp - now < 60
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct AuthMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
}

/// Pull `resource_metadata="..."` out of a `WWW-Authenticate` header value
/// (RFC 9728 §5.1). `None` if the header is absent or doesn't carry the
/// param — callers fall back to guessing the well-known path from the
/// server's own URL.
pub fn extract_resource_metadata(www_authenticate: &str) -> Option<String> {
    let re = regex::Regex::new(r#"resource_metadata="([^"]+)""#).expect("hardcoded regex is valid");
    re.captures(www_authenticate).map(|c| c[1].to_string())
}

/// RFC 8414 / RFC 9728 style: the well-known segment is inserted *before*
/// any existing path component (issuer `https://x/tenant1` → well-known URL
/// `https://x/.well-known/{name}/tenant1`).
fn insert_before_path(base: &Url, well_known: &str) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("/.well-known/{well_known}{path}"));
    url
}

/// Bare-origin fallback for protected-resource metadata: some resource
/// servers publish it at the root well-known path without repeating their
/// own path suffix.
fn well_known_root(base: &Url, well_known: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("/.well-known/{well_known}"));
    url
}

/// OpenID Connect Discovery style: the well-known segment is appended
/// *after* the existing path (issuer `https://x/tenant1` → discovery URL
/// `https://x/tenant1/.well-known/openid-configuration`). Deliberately a
/// different insertion rule than `insert_before_path` — OAuth AS metadata
/// (RFC 8414) and OIDC discovery disagree on this, a real interop wrinkle,
/// not an oversight.
fn append_after_path(base: &Url, well_known: &str) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}/.well-known/{well_known}"));
    url
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(client: &reqwest::Client, url: &Url) -> AppResult<T> {
    let resp = client.get(url.clone()).header("Accept", "application/json").send().await?;
    if !resp.status().is_success() {
        return Err(AppError::Mcp(format!("GET {url} returned {}", resp.status())));
    }
    resp.json::<T>().await.map_err(|e| AppError::Mcp(format!("Failed to parse JSON from {url}: {e}")))
}

/// Discover the authorization endpoints for an MCP server at `base_url`.
/// `hint` is the `resource_metadata` URL from a `WWW-Authenticate` header,
/// if the 401 that triggered this carried one.
async fn discover(client: &reqwest::Client, base_url: &str, hint: Option<&str>) -> AppResult<AuthMetadata> {
    let base = Url::parse(base_url)
        .map_err(|e| AppError::Mcp(format!("Invalid MCP server URL '{base_url}': {e}")))?;

    let resource_metadata_url = match hint.and_then(|h| Url::parse(h).ok()) {
        Some(u) => u,
        None => insert_before_path(&base, "oauth-protected-resource"),
    };

    let authorization_servers = match fetch_json::<ProtectedResourceMetadata>(client, &resource_metadata_url).await {
        Ok(meta) if !meta.authorization_servers.is_empty() => meta.authorization_servers,
        _ => {
            let root_url = well_known_root(&base, "oauth-protected-resource");
            match fetch_json::<ProtectedResourceMetadata>(client, &root_url).await {
                Ok(meta) if !meta.authorization_servers.is_empty() => meta.authorization_servers,
                // Last resort: the MCP server doesn't implement RFC 9728 at
                // all — assume it's its own authorization server.
                _ => vec![base_url.to_string()],
            }
        }
    };
    let as_origin = &authorization_servers[0];
    let as_base = Url::parse(as_origin)
        .map_err(|e| AppError::Mcp(format!("Invalid authorization server URL '{as_origin}': {e}")))?;

    let as_meta = match fetch_json::<AuthorizationServerMetadata>(client, &insert_before_path(&as_base, "oauth-authorization-server")).await {
        Ok(m) => m,
        Err(_) => fetch_json::<AuthorizationServerMetadata>(client, &append_after_path(&as_base, "openid-configuration")).await
            .map_err(|e| AppError::Mcp(format!("Could not discover OAuth metadata for {as_origin}: {e}")))?,
    };

    Ok(AuthMetadata {
        authorization_endpoint: as_meta.authorization_endpoint,
        token_endpoint: as_meta.token_endpoint,
        registration_endpoint: as_meta.registration_endpoint,
    })
}

#[derive(serde::Serialize)]
struct ClientRegistrationRequest<'a> {
    redirect_uris: [&'a str; 1],
    client_name: &'a str,
    grant_types: [&'a str; 2],
    response_types: [&'a str; 1],
    token_endpoint_auth_method: &'a str,
}

#[derive(Deserialize)]
struct ClientRegistrationResponse {
    client_id: String,
}

/// RFC 7591 dynamic client registration — registers `pooprusteek` as a
/// public client (no secret; PKCE carries the proof-of-possession instead,
/// standard practice for native/desktop apps per RFC 8252).
async fn register_client(client: &reqwest::Client, registration_endpoint: &str, redirect_uri: &str) -> AppResult<String> {
    let body = ClientRegistrationRequest {
        redirect_uris: [redirect_uri],
        client_name: "pooprusteek",
        grant_types: ["authorization_code", "refresh_token"],
        response_types: ["code"],
        token_endpoint_auth_method: "none",
    };
    let resp = client.post(registration_endpoint).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Mcp(format!("Dynamic client registration failed ({status}): {text}")));
    }
    resp.json::<ClientRegistrationResponse>().await
        .map(|r| r.client_id)
        .map_err(|e| AppError::Mcp(format!("Failed to parse client registration response: {e}")))
}

/// 3 concatenated UUIDv4s (48 random bytes), base64url-encoded — comfortably
/// within PKCE's required 43-128 char `code_verifier` range without adding a
/// `rand` dependency just for this.
fn generate_verifier() -> String {
    let mut bytes = Vec::with_capacity(48);
    for _ in 0..3 {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> AppResult<Url> {
    let mut url = Url::parse(authorization_endpoint)
        .map_err(|e| AppError::Mcp(format!("Invalid authorization endpoint '{authorization_endpoint}': {e}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url)
}

/// Wait for exactly one connection on the local loopback listener, parse the
/// `GET /callback?...` request line, answer with a minimal "close this
/// window" HTML page, and return the `code` query parameter after validating
/// `state`. 5-minute timeout — long enough for a human to actually log in.
async fn await_callback(listener: TcpListener, expected_state: &str) -> AppResult<String> {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(300), listener.accept())
        .await
        .map_err(|_| AppError::Mcp("Timed out waiting for the OAuth callback (5 min)".to_string()))??;

    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    // Drain the remaining request headers (up to the blank line) — we don't
    // need them, but the client expects them read before it'll consider the
    // request sent.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line.trim().is_empty() {
            break;
        }
    }

    let path = request_line.split_whitespace().nth(1)
        .ok_or_else(|| AppError::Mcp("Malformed OAuth callback request".to_string()))?;
    // Parsed via `Url` (not hand-rolled percent-decoding) so multi-byte
    // UTF-8 in e.g. `error_description` decodes correctly instead of being
    // split byte-by-byte.
    let full = Url::parse(&format!("http://127.0.0.1{path}"))
        .map_err(|e| AppError::Mcp(format!("Malformed OAuth callback path: {e}")))?;
    let params: HashMap<String, String> = full.query_pairs().into_owned().collect();

    let html = "<html><body>Authorization complete \u{2014} you can close this window.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html,
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    if let Some(err) = params.get("error") {
        return Err(AppError::Mcp(format!("Authorization server returned error: {err}")));
    }
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Err(AppError::Mcp("OAuth state mismatch (possible CSRF) \u{2014} aborting".to_string()));
    }
    params.get("code").cloned()
        .ok_or_else(|| AppError::Mcp("OAuth callback missing 'code' parameter".to_string()))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    code_verifier: &str,
) -> AppResult<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    post_form(client, token_endpoint, &params).await
}

async fn post_form(client: &reqwest::Client, url: &str, params: &[(&str, &str)]) -> AppResult<TokenResponse> {
    let resp = client.post(url).form(params).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Mcp(format!("Token request to {url} failed ({status}): {text}")));
    }
    resp.json::<TokenResponse>().await.map_err(|e| AppError::Mcp(format!("Failed to parse token response from {url}: {e}")))
}

fn http_client() -> AppResult<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?)
}

fn token_response_to_set(resp: TokenResponse, token_endpoint: String, client_id: String) -> TokenSet {
    let expires_at = resp.expires_in.map(|secs| unix_now() + secs);
    TokenSet {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at,
        token_endpoint,
        client_id,
    }
}

/// Run the full authorization-code + PKCE flow for `base_url` (an MCP
/// server's own HTTP/SSE URL) and return the resulting tokens. `hint` is the
/// `resource_metadata` URL carried by the 401 that triggered this, if any.
///
/// Fails with a clear error (rather than silently no-op'ing) if the
/// discovered authorization server has no `registration_endpoint` — manual
/// pre-registered `client_id` configuration is a reasonable follow-up but
/// isn't supported yet.
pub async fn run_flow(base_url: &str, hint: Option<String>) -> AppResult<TokenSet> {
    let client = http_client()?;

    let listener = TcpListener::bind("127.0.0.1:0").await
        .map_err(|e| AppError::Mcp(format!("Failed to bind local OAuth callback listener: {e}")))?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let metadata = discover(&client, base_url, hint.as_deref()).await?;
    let Some(registration_endpoint) = &metadata.registration_endpoint else {
        return Err(AppError::Mcp(
            "Server's authorization server does not support dynamic client registration (RFC 7591) \u{2014} manual client_id configuration is not supported yet".to_string(),
        ));
    };
    let client_id = register_client(&client, registration_endpoint, &redirect_uri).await?;

    let code_verifier = generate_verifier();
    let challenge = code_challenge_s256(&code_verifier);
    let state = uuid::Uuid::new_v4().to_string();

    let auth_url = authorization_url(&metadata.authorization_endpoint, &client_id, &redirect_uri, &state, &challenge)?;
    open::that(auth_url.as_str()).map_err(|e| AppError::Mcp(format!("Failed to open browser: {e}")))?;

    let code = await_callback(listener, &state).await?;
    let token_resp = exchange_code(&client, &metadata.token_endpoint, &code, &redirect_uri, &client_id, &code_verifier).await?;

    Ok(token_response_to_set(token_resp, metadata.token_endpoint, client_id))
}

/// Exchange a refresh token for a fresh access token at the same
/// `token_endpoint`/`client_id` recorded in `tokens`. Falls back to the
/// previous `refresh_token` if the server doesn't rotate it (many don't).
pub async fn refresh(tokens: &TokenSet) -> AppResult<TokenSet> {
    let Some(refresh_token) = &tokens.refresh_token else {
        return Err(AppError::Mcp("No refresh token available".to_string()));
    };
    let client = http_client()?;
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", tokens.client_id.as_str()),
    ];
    let resp = post_form(&client, &tokens.token_endpoint, &params).await?;
    let mut set = token_response_to_set(resp, tokens.token_endpoint.clone(), tokens.client_id.clone());
    if set.refresh_token.is_none() {
        set.refresh_token = tokens.refresh_token.clone();
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_resource_metadata_parses_quoted_param() {
        let header = r#"Bearer resource_metadata="https://example.com/.well-known/oauth-protected-resource""#;
        assert_eq!(
            extract_resource_metadata(header),
            Some("https://example.com/.well-known/oauth-protected-resource".to_string())
        );
    }

    #[test]
    fn extract_resource_metadata_none_when_absent() {
        assert_eq!(extract_resource_metadata("Bearer realm=\"mcp\""), None);
        assert_eq!(extract_resource_metadata(""), None);
    }

    #[test]
    fn insert_before_path_prefixes_well_known_before_existing_path() {
        let base = Url::parse("https://example.com/tenant1").unwrap();
        let url = insert_before_path(&base, "oauth-authorization-server");
        assert_eq!(url.as_str(), "https://example.com/.well-known/oauth-authorization-server/tenant1");
    }

    #[test]
    fn insert_before_path_no_existing_path_is_bare_well_known() {
        let base = Url::parse("https://example.com").unwrap();
        let url = insert_before_path(&base, "oauth-protected-resource");
        assert_eq!(url.as_str(), "https://example.com/.well-known/oauth-protected-resource");
    }

    #[test]
    fn append_after_path_suffixes_well_known_after_existing_path() {
        let base = Url::parse("https://example.com/tenant1").unwrap();
        let url = append_after_path(&base, "openid-configuration");
        assert_eq!(url.as_str(), "https://example.com/tenant1/.well-known/openid-configuration");
    }

    #[test]
    fn code_challenge_s256_matches_known_pkce_test_vector() {
        // RFC 7636 appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(code_challenge_s256(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generate_verifier_is_within_pkce_length_bounds() {
        let v = generate_verifier();
        assert!(v.len() >= 43 && v.len() <= 128, "verifier length {} out of PKCE bounds", v.len());
    }

    #[test]
    fn generate_verifier_is_random_each_call() {
        assert_ne!(generate_verifier(), generate_verifier());
    }

    #[test]
    fn token_set_is_expiring_soon_true_when_near_expiry() {
        let set = TokenSet {
            access_token: "a".to_string(),
            refresh_token: None,
            expires_at: Some(unix_now() + 30),
            token_endpoint: "https://x/token".to_string(),
            client_id: "c".to_string(),
        };
        assert!(set.is_expiring_soon());
    }

    #[test]
    fn token_set_is_expiring_soon_false_when_far_from_expiry() {
        let set = TokenSet {
            access_token: "a".to_string(),
            refresh_token: None,
            expires_at: Some(unix_now() + 3600),
            token_endpoint: "https://x/token".to_string(),
            client_id: "c".to_string(),
        };
        assert!(!set.is_expiring_soon());
    }

    #[test]
    fn token_set_is_expiring_soon_false_when_expiry_unknown() {
        let set = TokenSet {
            access_token: "a".to_string(),
            refresh_token: None,
            expires_at: None,
            token_endpoint: "https://x/token".to_string(),
            client_id: "c".to_string(),
        };
        assert!(!set.is_expiring_soon());
    }

    #[test]
    fn authorization_url_carries_all_required_params() {
        let url = authorization_url(
            "https://auth.example.com/authorize",
            "client-123",
            "http://127.0.0.1:4321/callback",
            "state-abc",
            "challenge-xyz",
        ).unwrap();
        let pairs: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(pairs.get("client_id").map(String::as_str), Some("client-123"));
        assert_eq!(pairs.get("redirect_uri").map(String::as_str), Some("http://127.0.0.1:4321/callback"));
        assert_eq!(pairs.get("state").map(String::as_str), Some("state-abc"));
        assert_eq!(pairs.get("code_challenge").map(String::as_str), Some("challenge-xyz"));
        assert_eq!(pairs.get("code_challenge_method").map(String::as_str), Some("S256"));
    }
}
