use crate::config::Config;
use crate::error::AppResult;
use crate::provider::ChatMessage;
use serde::{Deserialize, Serialize};

pub const SESSION_VERSION: i32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: i32,
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub workspace_root: String,
    pub model_type: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tag: Option<String>,
    /// The DeepSeek-side (remote) session id this local file last
    /// successfully synced with, so a future load can try to continue the
    /// same server-side thread instead of silently starting a new one.
    /// `None` if no turn has completed yet, or after the remote side was
    /// confirmed gone (see `broken`).
    #[serde(default)]
    pub provider_session_id: Option<String>,
    /// Last known DeepSeek `parent_message_id` for `provider_session_id`,
    /// needed to keep replies attached to the right thread branch on resume.
    #[serde(default)]
    pub provider_parent_message_id: Option<i64>,
    /// Set once a previously-linked remote session is confirmed unreachable
    /// (deleted/expired upstream). Surfaced in `/sessions` as a flagged
    /// entry; loading it again replays local history into a fresh remote
    /// session rather than re-checking a link already known dead.
    #[serde(default)]
    pub broken: bool,
}

/// Non-identity fields of [`Session`] that `save_session` needs but doesn't
/// derive from its other arguments — bundled so that function doesn't grow a
/// new positional parameter (and every call site) each time one more of
/// these is added.
#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    pub tag: Option<String>,
    pub broken: bool,
    pub provider_session_id: Option<String>,
    pub provider_parent_message_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub workspace_root: String,
    pub message_count: usize,
    pub title: String,
    pub model_type: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub broken: bool,
}

pub fn create_session_id() -> String {
    let now = chrono::Utc::now();
    let ts = now.format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
    let uuid_string = uuid::Uuid::new_v4().to_string();
    let suffix = uuid_string.split('-').next().unwrap_or("abc123");
    format!("{ts}-{suffix}")
}

pub fn save_session(
    id: &str,
    created_at: &str,
    messages: &[ChatMessage],
    config: &Config,
    workspace_root: &str,
    meta: &SessionMeta,
) -> AppResult<()> {
    let dir = Config::sessions_dir();
    std::fs::create_dir_all(&dir)?;

    let now = chrono::Utc::now().to_rfc3339();
    let session = Session {
        version: SESSION_VERSION,
        id: id.to_string(),
        created_at: created_at.to_string(),
        updated_at: now,
        workspace_root: workspace_root.to_string(),
        model_type: config.provider.model.clone(),
        messages: messages.to_vec(),
        tag: meta.tag.clone(),
        provider_session_id: meta.provider_session_id.clone(),
        provider_parent_message_id: meta.provider_parent_message_id,
        broken: meta.broken,
    };

    let path = dir.join(format!("{id}.json"));
    let json = serde_json::to_string_pretty(&session)?;
    crate::util::atomic_write(&path, json.as_bytes())?;
    Ok(())
}

pub fn load_local(id: &str, _config: &Config) -> AppResult<Session> {
    let dir = Config::sessions_dir();
    let path = dir.join(format!("{id}.json"));
    if !path.exists() {
        return Err(crate::error::AppError::SessionNotFound(id.to_string()));
    }
    let json = std::fs::read_to_string(&path)?;
    let session: Session = serde_json::from_str(&json)?;
    if session.version != SESSION_VERSION {
        return Err(crate::error::AppError::Custom(format!(
            "session format v{} is not supported by this build (expected v{SESSION_VERSION})",
            session.version
        )));
    }
    Ok(session)
}

pub fn list_sessions() -> AppResult<Vec<SessionSummary>> {
    let dir = Config::sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions: Vec<SessionSummary> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let json = match std::fs::read_to_string(&path) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("Failed to read session file {}: {e}", path.display());
                continue;
            }
        };
        let session: Session = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to parse session file {}: {e}", path.display());
                continue;
            }
        };
        let title = derive_title(&session.messages);
        sessions.push(SessionSummary {
            id: session.id,
            created_at: session.created_at,
            updated_at: session.updated_at,
            workspace_root: session.workspace_root,
            message_count: session.messages.len(),
            title,
            model_type: session.model_type,
            tag: session.tag,
            broken: session.broken,
        });
    }

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

/// Does a locally stored session file exist for this id? Cheaper than
/// `load_local` (no read/parse) and indifferent to version mismatches —
/// `/delete` must offer to remove even files this build can't load.
pub fn local_exists(id: &str) -> bool {
    Config::sessions_dir().join(format!("{id}.json")).exists()
}

/// Delete a locally stored session file. Returns `false` when no such file
/// existed (not an error — `/delete` treats "already gone" as success).
pub fn delete_local(id: &str, _config: &Config) -> AppResult<bool> {
    let path = Config::sessions_dir().join(format!("{id}.json"));
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

pub fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn save_local(session: &Session, _config: &Config) -> AppResult<()> {
    let dir = Config::sessions_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)?;
    crate::util::atomic_write(&path, json.as_bytes())?;
    Ok(())
}

pub fn derive_title(messages: &[ChatMessage]) -> String {
    for msg in messages.iter().rev() {
        let text = match msg.role {
            crate::provider::Role::User | crate::provider::Role::Assistant => {
                msg.visible_content()
            }
            _ => continue,
        };
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            let first_line = trimmed.lines().next().unwrap_or(trimmed);
            if first_line.chars().count() > 80 {
                let truncated: String = first_line.chars().take(80).collect();
                return format!("{}...", truncated);
            }
            return first_line.to_string();
        }
    }
    "Empty conversation".to_string()
}

pub fn history_path() -> std::path::PathBuf {
    Config::data_dir().join("history.json")
}

pub fn load_history() -> Vec<String> {
    let path = history_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn append_history(input: &str) {
    let mut history = load_history();
    if history.last().map(|s| s.as_str()) != Some(input) {
        history.push(input.to_string());
    }
    if history.len() > 500 {
        history.drain(0..history.len() - 500);
    }
    if let Ok(json) = serde_json::to_string(&history) {
        let _ = crate::util::atomic_write(&history_path(), json.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `contents` directly to `Config::sessions_dir()/{id}.json`,
    /// bypassing `save_local`, so we can plant a session with an arbitrary
    /// (including wrong) `version` field. `id` should be unique per test to
    /// avoid colliding with other tests or a real user's sessions, since
    /// `Config::sessions_dir()` is a fixed, non-injectable path.
    fn plant_session_file(id: &str, contents: &str) -> std::path::PathBuf {
        let dir = Config::sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.json"));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_local_rejects_future_session_version() {
        let id = "pooprusteek_test_session_future_version";
        let future_version = SESSION_VERSION + 1;
        let json = format!(
            r#"{{"version":{future_version},"id":"{id}","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","workspace_root":"/tmp","model_type":"deepseek-chat","messages":[]}}"#
        );
        let path = plant_session_file(id, &json);

        let config = Config::default();
        let result = load_local(id, &config);

        cleanup(&path);

        let err = result.expect_err("session with a newer version must be rejected");
        let msg = err.to_string();
        assert!(msg.contains(&future_version.to_string()), "error should mention the file's version: {msg}");
        assert!(msg.contains(&SESSION_VERSION.to_string()), "error should mention the expected version: {msg}");
    }

    #[test]
    fn load_local_accepts_current_session_version() {
        let id = "pooprusteek_test_session_current_version";
        let json = format!(
            r#"{{"version":{SESSION_VERSION},"id":"{id}","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","workspace_root":"/tmp","model_type":"deepseek-chat","messages":[]}}"#
        );
        let path = plant_session_file(id, &json);

        let config = Config::default();
        let result = load_local(id, &config);

        cleanup(&path);

        let session = result.expect("session at the current version must load");
        assert_eq!(session.version, SESSION_VERSION);
        assert_eq!(session.id, id);
        // The JSON above predates provider_session_id/broken — `#[serde(default)]`
        // must fill them in rather than fail to parse, or every session file
        // written before this feature would become unloadable.
        assert_eq!(session.provider_session_id, None);
        assert_eq!(session.provider_parent_message_id, None);
        assert!(!session.broken);
    }

    #[test]
    fn save_session_round_trips_provider_identity_and_broken_flag() {
        let id = "pooprusteek_test_session_meta_roundtrip";
        let config = Config::default();
        let meta = SessionMeta {
            tag: Some("Imported".to_string()),
            broken: true,
            provider_session_id: Some("remote-123".to_string()),
            provider_parent_message_id: Some(9),
        };

        save_session(id, "2020-01-01T00:00:00Z", &[], &config, "/tmp", &meta)
            .expect("save_session should succeed");
        let path = Config::sessions_dir().join(format!("{id}.json"));

        let loaded = load_local(id, &config);
        cleanup(&path);

        let session = loaded.expect("just-saved session must load back");
        assert_eq!(session.tag.as_deref(), Some("Imported"));
        assert!(session.broken);
        assert_eq!(session.provider_session_id.as_deref(), Some("remote-123"));
        assert_eq!(session.provider_parent_message_id, Some(9));
    }

    #[test]
    fn list_sessions_skips_unparseable_files_without_erroring() {
        let id = "pooprusteek_test_session_corrupt";
        let path = plant_session_file(id, "{ this is not valid json");

        let _config = Config::default();
        let result = list_sessions();

        cleanup(&path);

        // A single corrupt file must not fail the whole listing — it should
        // be skipped (and, per session.rs, warned about via tracing).
        assert!(result.is_ok(), "list_sessions should not error on a corrupt file: {result:?}");
    }
}
