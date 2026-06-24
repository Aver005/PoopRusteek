use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PromptFiles {
    pub base_prompt: String,
    pub tools_prompt: String,
}

pub fn load_prompt_files() -> AppResult<PromptFiles> {
    let base_prompt = std::fs::read_to_string(resolve_existing_asset_path("assets/prompts/base.prompt.md")?)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let tools_prompt = std::fs::read_to_string(resolve_existing_asset_path("assets/prompts/tools.prompt.md")?)
        .map_err(|error| AppError::Config(error.to_string()))?;

    Ok(PromptFiles {
        base_prompt,
        tools_prompt,
    })
}

pub fn resolve_existing_asset_path(relative_path: &str) -> AppResult<PathBuf> {
    let candidates = collect_asset_candidates(relative_path);

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(AppError::Config(format!(
        "Asset not found for {relative_path}"
    )))
}

fn collect_asset_candidates(relative_path: &str) -> Vec<PathBuf> {
    let sanitized = relative_path.trim_start_matches(['.', '/', '\\']);
    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(sanitized));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join(sanitized));
            }
        }
    }

    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(sanitized));

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(sanitized));
    }

    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    unique
}
