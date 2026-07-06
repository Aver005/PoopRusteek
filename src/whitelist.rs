use crate::config::Config;
use crate::error::AppResult;
use std::collections::HashSet;

const WHITELIST_FILE: &str = "whitelist.json";

fn whitelist_path() -> std::path::PathBuf {
    Config::data_dir().join(WHITELIST_FILE)
}

pub fn load() -> HashSet<String> {
    let path = whitelist_path();
    if !path.exists() {
        return HashSet::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("Failed to read whitelist: {e}");
            HashSet::new()
        }
    }
}

pub fn save(tools: &HashSet<String>) -> AppResult<()> {
    let path = whitelist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(tools)?;
    crate::util::atomic_write(&path, content.as_bytes())?;
    Ok(())
}

/// Add one tool to the persisted whitelist (read-modify-write). Used by the
/// approval modal's "always allow" so the choice survives both the
/// `/whitelist` picker (which reloads from this file) and restarts.
pub fn persist_approval(tool_name: &str) {
    let mut tools = load();
    if tools.insert(tool_name.to_string())
        && let Err(e) = save(&tools)
    {
        tracing::warn!("Failed to persist tool approval for {tool_name}: {e}");
    }
}
