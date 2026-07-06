/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8 char.
/// Returns a slice ending on a char boundary. Use instead of `&s[..n]` for any
/// content that may contain multibyte UTF-8 (PTY/ANSI output, JSON bodies, etc).
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    &s[..s.floor_char_boundary(max_bytes)]
}

/// Truncate to `max_bytes` with an ellipsis suffix, char-boundary safe.
pub fn truncate_with_ellipsis(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = max_bytes.saturating_sub(3);
    let head = truncate_at_char_boundary(s, cut);
    format!("{head}...")
}

/// Write `contents` to `path` atomically: write to a sibling `.tmp` file, then
/// rename over the target. A crash mid-write leaves the old file intact instead
/// of a truncated one. Use for every persisted file (sessions, config,
/// whitelist, mcp.json) — never `std::fs::write` directly on user data.
pub fn atomic_write(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension({
        let mut ext = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        ext.push_str(".tmp");
        ext
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, contents)?;
    // On Windows, `rename` fails if the target exists; remove it first. The
    // window between remove and rename is smaller than a truncate-in-place.
    #[cfg(windows)]
    {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::rename(&tmp, path)
}

/// Expand a leading `~` or `~/` to the user's home directory. Plain `~foo`
/// (other-user syntax) is returned unchanged. The single shared impl — do not
/// hand-roll tilde handling at call sites (two past copies were both wrong).
pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = std::env::temp_dir().join("pooprusteek_util_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("data.json");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_tilde_handles_home_and_passthrough() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x/y"), home.join("x/y"));
        assert_eq!(
            expand_tilde("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde("rel/path"),
            std::path::PathBuf::from("rel/path")
        );
    }
}

/// Render one tool definition (name, description, parameter list from a
/// JSON schema) as the markdown-ish block used in the system prompt, the
/// `/tools` listing, and semantic tool hints. Required params sort first.
pub fn format_tool_definition(name: &str, description: &str, schema: &serde_json::Value) -> String {
    let mut result = format!("- `{name}`: {description}");

    if let Some(props) = schema.get("properties").and_then(|p| p.as_object())
        && !props.is_empty()
    {
        result.push_str("\n  Parameters:");
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        let mut params: Vec<(&String, &serde_json::Value)> = props.iter().collect();
        params.sort_by(|a, b| {
            let a_req = required.contains(a.0);
            let b_req = required.contains(b.0);
            b_req.cmp(&a_req).then(a.0.cmp(b.0))
        });
        for (param_name, param_info) in params {
            let param_type = param_info
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any");
            let param_desc = param_info
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let req_str = if required.contains(param_name) {
                "required"
            } else {
                "optional"
            };
            result.push_str(&format!(
                "\n    \u{2022} `{param_name}` ({param_type}, {req_str}): {param_desc}"
            ));
        }
    }

    result
}
