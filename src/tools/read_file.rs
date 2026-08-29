use super::*;

/// Read a UTF-8 text file from disk, with 1-based line paging.
///
/// This is the model's escape hatch for the context-compaction ladder: once
/// a tool result gets replaced with a marker naming a file on disk, this is
/// how the model fetches the real content back.
pub struct ReadFileTool;

const DEFAULT_LIMIT: usize = 400;

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description:
                "Read a UTF-8 text file from disk, with paging (offset/limit over lines). This is how you fetch the full content of a tool result that was cleared from the conversation and replaced with a file-path marker."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the text file"
                    },
                    "offset": {
                        "type": "number",
                        "description": "1-based line number to start from (default 1)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Max lines to return (default 400)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(path) = args["path"].as_str().filter(|p| !p.trim().is_empty()) else {
            return ToolResult::error("Missing 'path' argument");
        };

        // Absurd input is normalised rather than rejected: offset below 1
        // clamps to 1, limit at or below 0 falls back to the default.
        let offset = parse_arg(&args, "offset").filter(|&v| v > 0).unwrap_or(1) as usize;
        let limit = parse_arg(&args, "limit")
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_LIMIT as i64) as usize;

        let path_owned = path.to_string();
        // File I/O is blocking; keep it off the async worker (invariant 9).
        let result =
            tokio::task::spawn_blocking(move || read_file_blocking(&path_owned, offset, limit))
                .await
                .unwrap_or_else(|e| Err(format!("Internal error reading file: {e}")));

        match result {
            Ok(body) => ToolResult::success(&body),
            Err(msg) => ToolResult::error(&msg),
        }
    }
}

/// Accepts JSON integers or floats — LLMs sometimes emit `3.0` for a count.
fn parse_arg(args: &Value, key: &str) -> Option<i64> {
    args[key]
        .as_i64()
        .or_else(|| args[key].as_f64().map(|f| f as i64))
}

/// Reads the whole file into memory, then slices by line. Simpler than a
/// streaming reader and fine at the sizes this tool is meant for; it stops
/// being fine only for files far larger than what a model should page through.
fn read_file_blocking(path_str: &str, offset: usize, limit: usize) -> Result<String, String> {
    // Разворачиваем `~` так же, как `edit`/`write`: иначе один и тот же путь
    // читается с ошибкой, а пишется успешно.
    let path = crate::util::expand_tilde(path_str);
    ensure_regular_file(&path, path_str)?;

    let bytes = std::fs::read(&path).map_err(|e| classify_io_error(path_str, &e))?;
    let text = crate::util::decode_process_output(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let start = offset.saturating_sub(1).min(total);
    let end = (start + limit).min(total);
    let slice = lines[start..end].join("\n");

    let header = if end < total {
        format!(
            "{path_str} (lines {}-{} of {total}; more remain — call again with offset={} to continue)\n",
            start + 1,
            end,
            end + 1
        )
    } else {
        format!("{path_str} (lines {}-{} of {total})\n", start + 1, end)
    };
    Ok(format!("{header}{slice}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek_read_file_test_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn small_file_returns_content_with_header() {
        let dir = temp_dir("small");
        let path = dir.join("small.txt");
        std::fs::write(&path, "hello\nworld").unwrap();

        let result = read_file_blocking(path.to_str().unwrap(), 1, 400).unwrap();
        assert!(result.starts_with(path.to_str().unwrap()));
        assert!(result.contains("hello"));
        assert!(result.contains("world"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paging_returns_slice_and_notes_more_remain() {
        let dir = temp_dir("paging");
        let path = dir.join("lines.txt");
        let content: String = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let result = read_file_blocking(path.to_str().unwrap(), 3, 4).unwrap();
        assert!(result.contains("line3"));
        assert!(result.contains("line6"));
        assert!(!result.contains("line7"));
        assert!(result.contains("more remain"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_is_error_not_panic() {
        let dir = temp_dir("missing");
        let path = dir.join("does_not_exist.txt");

        let result = read_file_blocking(path.to_str().unwrap(), 1, 400);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf16_file_decodes_readably() {
        let dir = temp_dir("utf16");
        let path = dir.join("utf16.txt");
        let mut bytes = vec![0xFF, 0xFE]; // UTF-16 LE BOM
        for unit in "hello\nworld".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let result = read_file_blocking(path.to_str().unwrap(), 1, 400).unwrap();
        assert!(result.contains("hello"));
        assert!(result.contains("world"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
