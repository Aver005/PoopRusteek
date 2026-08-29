pub mod background;
pub mod edit;
pub mod history_search;
pub mod question;
pub mod read_file;
pub mod registry;
pub mod shell;
pub mod shell_control;
pub mod skill;
pub mod task;
pub mod timer;
pub mod tool_search;

use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

/// Tool names the agent loops special-case *before* registry dispatch (the
/// `question`/`task`/`timer` tools are declared like any other tool so the
/// model sees them, but are never executed through `ToolRegistry::execute`).
/// Dispatch sites must compare against these constants, not string
/// literals, so a rename can't silently break the special-casing.
pub const QUESTION_TOOL_NAME: &str = "question";
pub const TASK_TOOL_NAME: &str = "task";
pub const TIMER_TOOL_NAME: &str = "timer";

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: &str) -> Self {
        Self {
            content: content.to_string(),
            is_error: false,
        }
    }

    pub fn error(content: &str) -> Self {
        Self {
            content: content.to_string(),
            is_error: true,
        }
    }
}

pub fn looks_interactive_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "bun create",
        "npm create",
        "npx create",
        "npm init",
        "pnpm create",
        "yarn create",
        "gh auth",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub fn looks_persistent_background_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "vite",
        "next dev",
        "bun dev",
        "bun run dev",
        "npm run dev",
        "pnpm dev",
        "pnpm run dev",
        "yarn dev",
        "webpack serve",
        "cargo watch",
        "cargo leptos watch",
        "trunk serve",
        "serve ",
        "http-server",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Сколько строк аргумента показываем в модалке подтверждения.
const MAX_PREVIEW_LINES: usize = 40;
/// Предел длины строки там же: попап не переносит строки.
const MAX_PREVIEW_LINE_BYTES: usize = 120;

/// Что человек видит в модалке подтверждения. Сырой pretty-JSON не годится
/// для правки файлов: `serde_json` экранирует переводы строк, и содержимое
/// файла схлопывается в одну нечитаемую строку, а попап по ней не растёт.
pub fn approval_preview(name: &str, arguments: &Value) -> String {
    let path = arguments["path"].as_str().unwrap_or_default();
    match name {
        "write" => {
            let mut out = format!("path: {path}{}\n\n", outside_workspace_note(path));
            push_preview_body(
                &mut out,
                arguments["content"].as_str().unwrap_or_default(),
                ' ',
            );
            out
        }
        "edit" => {
            let all = arguments["replace_all"].as_bool().unwrap_or(false);
            let scope = if all { "  (replace_all)" } else { "" };
            let mut out = format!("path: {path}{}{scope}\n\n", outside_workspace_note(path));
            push_preview_body(
                &mut out,
                arguments["old_string"].as_str().unwrap_or_default(),
                '-',
            );
            push_preview_body(
                &mut out,
                arguments["new_string"].as_str().unwrap_or_default(),
                '+',
            );
            out
        }
        _ => serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string()),
    }
}

/// Запись за пределы рабочей папки — не запрет, но человек должен её заметить.
fn outside_workspace_note(path: &str) -> &'static str {
    if path.is_empty() {
        return "";
    }
    let target = crate::util::expand_tilde(path);
    let Ok(cwd) = std::env::current_dir() else {
        return "";
    };
    let absolute = if target.is_absolute() {
        target
    } else {
        cwd.join(target)
    };
    // Канонизация только для существующего пути; новый файл судим по родителю.
    let resolved = absolute.canonicalize().or_else(|_| {
        absolute
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(absolute)
            .canonicalize()
    });
    match (resolved, cwd.canonicalize()) {
        (Ok(resolved), Ok(cwd)) if !resolved.starts_with(&cwd) => "   ⚠ OUTSIDE WORKSPACE",
        _ => "",
    }
}

fn push_preview_body(out: &mut String, body: &str, marker: char) {
    if body.is_empty() {
        out.push_str(&format!("{marker} (empty)\n"));
        return;
    }
    let lines: Vec<&str> = body.lines().collect();
    for line in lines.iter().take(MAX_PREVIEW_LINES) {
        out.push_str(&format!(
            "{marker} {}\n",
            crate::util::truncate_with_ellipsis(line, MAX_PREVIEW_LINE_BYTES)
        ));
    }
    if lines.len() > MAX_PREVIEW_LINES {
        out.push_str(&format!(
            "{marker} … {} more line(s)\n",
            lines.len() - MAX_PREVIEW_LINES
        ));
    }
}

/// Ошибка файлового ввода-вывода словами, понятными модели. Живёт здесь, а не
/// в `util`: это текст для LLM, а не байтовый примитив.
pub fn classify_io_error(path_str: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("File not found: {path_str}"),
        std::io::ErrorKind::PermissionDenied => format!("Permission denied: {path_str}"),
        _ => format!("Failed to access {path_str}: {e}"),
    }
}

/// Путь существует и это обычный файл. Каталог отсекаем отдельно: иначе
/// `fs::read` вернёт «Access denied» и модель уйдёт чинить права.
pub fn ensure_regular_file(path: &std::path::Path, path_str: &str) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|e| classify_io_error(path_str, &e))?;
    if metadata.is_dir() {
        return Err(format!("{path_str} is a directory, not a file"));
    }
    Ok(())
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, args: Value) -> ToolResult;
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_write_preview_keeps_real_line_breaks() {
        // Ровно тот дефект, ради которого функция и появилась: pretty-JSON
        // отдавал экранированный перевод строки, и попап не рос по высоте.
        let preview = approval_preview(
            "write",
            &json!({"path": "a.rs", "content": "line one\nline two\nline three"}),
        );
        assert!(preview.lines().count() >= 5, "{preview}");
        assert!(preview.contains("path: a.rs"), "{preview}");
        assert!(preview.contains("  line two"), "{preview}");
        assert!(
            !preview.contains(r"\n"),
            "перевод строки остался экранированным"
        );
    }

    #[test]
    fn an_edit_preview_shows_both_sides_as_a_diff() {
        let preview = approval_preview(
            "edit",
            &json!({"path": "a.rs", "old_string": "old", "new_string": "new"}),
        );
        assert!(preview.contains("- old"), "{preview}");
        assert!(preview.contains("+ new"), "{preview}");
    }

    #[test]
    fn a_bulk_edit_is_flagged_in_the_preview() {
        let preview = approval_preview(
            "edit",
            &json!({"path": "a.rs", "old_string": "=", "new_string": "= ", "replace_all": true}),
        );
        assert!(preview.contains("replace_all"), "{preview}");
    }

    #[test]
    fn a_long_body_is_capped_with_a_counter() {
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let preview = approval_preview("write", &json!({"path": "a.rs", "content": body}));
        assert!(preview.contains("more line(s)"), "{preview}");
    }

    #[test]
    fn an_empty_body_says_so_instead_of_showing_nothing() {
        let preview = approval_preview("write", &json!({"path": "a.rs", "content": ""}));
        assert!(preview.contains("(empty)"), "{preview}");
    }

    #[test]
    fn other_tools_keep_the_json_preview() {
        let preview = approval_preview("bash", &json!({"command": "ls -a"}));
        assert!(preview.contains("\"command\""), "{preview}");
    }

    #[test]
    fn a_path_outside_the_workspace_is_flagged() {
        let outside = std::env::temp_dir().join("definitely-not-in-the-repo.txt");
        let preview = approval_preview(
            "write",
            &json!({"path": outside.to_str().unwrap(), "content": "x"}),
        );
        assert!(preview.contains("OUTSIDE WORKSPACE"), "{preview}");
    }

    #[test]
    fn a_path_inside_the_workspace_is_not_flagged() {
        let preview = approval_preview("write", &json!({"path": "src/main.rs", "content": "x"}));
        assert!(!preview.contains("OUTSIDE WORKSPACE"), "{preview}");
    }
}
