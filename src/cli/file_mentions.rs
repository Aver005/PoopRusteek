use std::path::{Path, PathBuf};

pub struct FileMention {
    pub path: PathBuf,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    pub content: String,
}

pub fn extract_mentions(input: &str, workspace: &Path) -> Vec<FileMention> {
    let mut mentions = Vec::new();

    for word in input.split_whitespace() {
        if !word.starts_with('@') {
            continue;
        }

        let path_str = &word[1..];
        let (path, line_range) = parse_path_with_lines(path_str);

        let full_path = if path.is_absolute() {
            path
        } else {
            workspace.join(&path)
        };

        if !full_path.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let (content, line_start, line_end) = if let Some((start, end)) = line_range {
            let lines: Vec<&str> = content.lines().collect();
            // Clamp both ends to the file's actual line count. `start` and
            // `end` come straight from user input (`@file.rs:500-600`) and
            // may exceed `lines.len()`, or `end` may be less than `start` for
            // a reversed range like `:30-10` — either previously panicked on
            // `lines[start_idx..end_idx]`. `end_idx.max(start_idx)` keeps the
            // range non-inverted so the slice is empty instead of panicking.
            let start_idx = start.saturating_sub(1).min(lines.len());
            let end_idx = end.min(lines.len()).max(start_idx);
            let sliced = if start_idx >= end_idx {
                format!("[range out of bounds: file has {} lines]", lines.len())
            } else {
                lines[start_idx..end_idx].join("\n")
            };
            (sliced, Some(start), Some(end))
        } else {
            (content, None, None)
        };

        mentions.push(FileMention {
            path: full_path,
            line_start,
            line_end,
            content,
        });
    }

    mentions
}

fn parse_path_with_lines(s: &str) -> (PathBuf, Option<(usize, usize)>) {
    if let Some(colon_pos) = s.rfind(':') {
        let path_part = &s[..colon_pos];
        let line_part = &s[colon_pos + 1..];

        if let Some(dash_pos) = line_part.find('-') {
            let start: usize = line_part[..dash_pos].parse().unwrap_or(0);
            let end: usize = line_part[dash_pos + 1..].parse().unwrap_or(0);
            if start > 0 && end > 0 {
                return (PathBuf::from(path_part), Some((start, end)));
            }
        } else if let Ok(line) = line_part.parse::<usize>()
            && line > 0
        {
            return (PathBuf::from(path_part), Some((line, line)));
        }

        (PathBuf::from(s), None)
    } else {
        (PathBuf::from(s), None)
    }
}

pub fn format_mention(mention: &FileMention) -> String {
    let header = match (mention.line_start, mention.line_end) {
        (Some(s), Some(e)) if s == e => format!("File: {} (line {})", mention.path.display(), s),
        (Some(s), Some(e)) => format!("File: {} (lines {}-{})", mention.path.display(), s, e),
        _ => format!("File: {}", mention.path.display()),
    };

    format!("{header}\n```\n{}\n```", mention.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a 100-line file (lines "line 1" .. "line 100") under a unique
    /// scratch dir and returns (workspace_dir, filename). Each test gets its
    /// own subdirectory so parallel test runs don't collide.
    fn write_100_line_file(test_name: &str) -> (PathBuf, &'static str) {
        let dir = std::env::temp_dir()
            .join("pooprusteek_file_mentions_test")
            .join(test_name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let filename = "hundred.txt";
        let content = (1..=100)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(filename), content).unwrap();
        (dir, filename)
    }

    #[test]
    fn out_of_range_end_does_not_panic() {
        let (dir, filename) = write_100_line_file("out_of_range_end");
        let input = format!("@{filename}:500-600");
        let mentions = extract_mentions(&input, &dir);
        assert_eq!(mentions.len(), 1);
        assert!(
            mentions[0].content.contains("range out of bounds"),
            "expected an out-of-bounds note, got: {}",
            mentions[0].content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reversed_range_does_not_panic() {
        let (dir, filename) = write_100_line_file("reversed_range");
        let input = format!("@{filename}:30-10");
        let mentions = extract_mentions(&input, &dir);
        assert_eq!(mentions.len(), 1);
        assert!(
            mentions[0].content.contains("range out of bounds"),
            "expected an out-of-bounds note, got: {}",
            mentions[0].content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exact_boundary_range_returns_full_slice() {
        let (dir, filename) = write_100_line_file("exact_boundary");
        // 1-100 on a 100-line file is exactly in range: no clamping needed,
        // no out-of-bounds note, and the full content should come through.
        let input = format!("@{filename}:1-100");
        let mentions = extract_mentions(&input, &dir);
        assert_eq!(mentions.len(), 1);
        assert!(!mentions[0].content.contains("range out of bounds"));
        assert!(mentions[0].content.starts_with("line 1\n"));
        assert!(mentions[0].content.ends_with("line 100"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn end_beyond_file_clamps_instead_of_panicking() {
        let (dir, filename) = write_100_line_file("end_beyond_file");
        // start is in range, end overruns — should clamp to the last line
        // rather than treat it as fully out of bounds.
        let input = format!("@{filename}:95-150");
        let mentions = extract_mentions(&input, &dir);
        assert_eq!(mentions.len(), 1);
        assert!(!mentions[0].content.contains("range out of bounds"));
        assert!(mentions[0].content.starts_with("line 95\n"));
        assert!(mentions[0].content.ends_with("line 100"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
