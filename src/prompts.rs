/// Embedded copies of the core prompts so an installed binary works with no
/// `assets/` folder nearby (same rationale as the embedded PoW wasm in
/// `provider::pow`). A debug build prefers the source checkout — see [`load_asset_text`].
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
        base_prompt: load_asset_text("prompts/base.prompt.md", EMBEDDED_BASE_PROMPT),
        tools_prompt: load_asset_text("prompts/tools.prompt.md", EMBEDDED_TOOLS_PROMPT),
        goal_evaluator_prompt: load_asset_text(
            "prompts/goal-evaluator.prompt.md",
            EMBEDDED_GOAL_EVALUATOR_PROMPT,
        ),
    }
}

/// Живая копия из исходников в debug-сборке, иначе встроенная.
fn load_asset_text(relative_path: &str, embedded: &str) -> String {
    let Some(path) = crate::util::dev_assets_dir().map(|dir| dir.join(relative_path)) else {
        return embedded.to_string();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                "Failed to read {}: {error}; using embedded copy",
                path.display()
            );
            embedded.to_string()
        }
    }
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

    #[test]
    fn debug_build_reads_the_source_checkout() {
        // Тесты — debug-сборка: файл из исходников совпадает со встроенным.
        let text = load_asset_text("prompts/base.prompt.md", "embedded-marker");
        assert_eq!(text, EMBEDDED_BASE_PROMPT);
        let missing = load_asset_text("prompts/no-such.prompt.md", "embedded-marker");
        assert_eq!(missing, "embedded-marker");
    }
}
