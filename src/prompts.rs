use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

/// Embedded copies of the core prompts so an installed binary works with no
/// `assets/` folder nearby (same rationale as the embedded PoW wasm in
/// `provider::pow`). A file on disk still wins — see [`load_asset_text`].
const EMBEDDED_BASE_PROMPT: &str = include_str!("../assets/prompts/base.prompt.md");
const EMBEDDED_TOOLS_PROMPT: &str = include_str!("../assets/prompts/tools.prompt.md");
const EMBEDDED_GOAL_EVALUATOR_PROMPT: &str =
    include_str!("../assets/prompts/goal-evaluator.prompt.md");

#[derive(Debug, Clone)]
pub struct PromptFiles {
    pub base_prompt: String,
    pub tools_prompt: String,
    pub goal_evaluator_prompt: String,
}

pub fn load_prompt_files() -> PromptFiles {
    PromptFiles {
        base_prompt: load_asset_text("assets/prompts/base.prompt.md", EMBEDDED_BASE_PROMPT),
        tools_prompt: load_asset_text("assets/prompts/tools.prompt.md", EMBEDDED_TOOLS_PROMPT),
        goal_evaluator_prompt: load_asset_text(
            "assets/prompts/goal-evaluator.prompt.md",
            EMBEDDED_GOAL_EVALUATOR_PROMPT,
        ),
    }
}

/// Read a prompt from disk when a copy exists (exe dir, repo checkout, or
/// cwd — allows local overrides), otherwise fall back to the embedded copy.
fn load_asset_text(relative_path: &str, embedded: &str) -> String {
    if let Ok(path) = resolve_existing_asset_path(relative_path) {
        match std::fs::read_to_string(&path) {
            Ok(text) => return text,
            Err(error) => tracing::warn!(
                "Failed to read {}: {error}; using embedded copy",
                path.display()
            ),
        }
    }
    embedded.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Prompt-bloat budget. The two core templates are fixed per-session
    /// overhead every DeepSeek session pays up front as flat text (the web
    /// API has no system role), and an oversized instruction block measurably
    /// degrades a weak model's adherence. Deliberate growth is fine — trim
    /// something else or bump the budget consciously in the same commit.
    #[test]
    fn core_prompt_templates_stay_within_byte_budget() {
        assert!(
            EMBEDDED_BASE_PROMPT.len() < 5_000,
            "base.prompt.md grew to {} bytes (budget 5000)",
            EMBEDDED_BASE_PROMPT.len()
        );
        // Поднято 4500 → 5500 вместе с секцией «План работы» (`todo`):
        // дисциплина обновления плана — это и есть сам инструмент, коротким
        // абзацем она не задаётся. Дальше — только за счёт урезания другого.
        assert!(
            EMBEDDED_TOOLS_PROMPT.len() < 5_500,
            "tools.prompt.md grew to {} bytes (budget 5500). Trim something \
             else rather than raising this number.",
            EMBEDDED_TOOLS_PROMPT.len()
        );
    }
}

fn collect_asset_candidates(relative_path: &str) -> Vec<PathBuf> {
    let sanitized = relative_path.trim_start_matches(['.', '/', '\\']);
    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(sanitized));
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join(sanitized));
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
