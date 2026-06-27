use crate::config::Config;
use crate::error::AppResult;
use crate::provider::ChatMessage;
use serde::{Deserialize, Serialize};

const SESSION_VERSION: i32 = 1;

#[derive(Debug, Serialize, Deserialize)]
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
        tag: None,
    };

    let path = dir.join(format!("{id}.json"));
    let json = serde_json::to_string_pretty(&session)?;
    std::fs::write(&path, json)?;
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
    Ok(session)
}

pub fn list_sessions(_config: &Config) -> AppResult<Vec<SessionSummary>> {
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
            Err(_) => continue,
        };
        let session: Session = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(_) => continue,
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
        });
    }

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

pub fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn save_local(session: &Session, _config: &Config) -> AppResult<()> {
    let dir = Config::sessions_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)?;
    std::fs::write(&path, json)?;
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
        let _ = std::fs::write(history_path(), json);
    }
}
