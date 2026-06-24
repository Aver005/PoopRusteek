use crate::config::Config;
use crate::error::AppResult;
use crate::provider::ChatMessage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub created_at: String,
    pub updated_at: String,
}

impl Session {
    pub fn new() -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            messages: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    pub fn save(&self, _config: &Config) -> AppResult<PathBuf> {
        let dir = Config::sessions_dir();
        std::fs::create_dir_all(&dir)?;

        let path = dir.join(format!("{}.json", self.id));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;

        Ok(path)
    }

    pub fn load(path: &PathBuf) -> AppResult<Self> {
        let json = std::fs::read_to_string(path)?;
        let session: Session = serde_json::from_str(&json)?;
        Ok(session)
    }

    pub fn list_sessions(_config: &Config) -> AppResult<Vec<PathBuf>> {
        let dir = Config::sessions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext == "json")
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .collect();

        sessions.sort_by(|a, b| {
            let ma = std::fs::metadata(a).ok().and_then(|m| m.modified().ok());
            let mb = std::fs::metadata(b).ok().and_then(|m| m.modified().ok());
            mb.cmp(&ma)
        });

        Ok(sessions)
    }

    pub fn delete(&self, _config: &Config) -> AppResult<()> {
        let path = Config::sessions_dir().join(format!("{}.json", self.id));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}
