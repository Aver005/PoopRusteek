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
            let start_idx = start.saturating_sub(1);
            let end_idx = end.min(lines.len());
            let sliced = lines[start_idx..end_idx].join("\n");
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
        } else if let Ok(line) = line_part.parse::<usize>() {
            if line > 0 {
                return (PathBuf::from(path_part), Some((line, line)));
            }
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
