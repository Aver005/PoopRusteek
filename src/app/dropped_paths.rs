//! Файлы, брошенные в терминал мышью: терминал вставляет их пути, у каждого в своём
//! формате. Перетаскивание — только если каждый кусок вставки существующий файл.

use std::path::PathBuf;

/// Больше путей за раз — это уже вывод `find`, а не перетаскивание.
const MAX_DROPPED: usize = 100;

/// Пути из вставки, если вся она — перечень существующих файлов.
pub fn parse(text: &str) -> Option<Vec<PathBuf>> {
    let tokens = tokenize(text.trim())?;
    if tokens.is_empty() || tokens.len() > MAX_DROPPED {
        return None;
    }
    tokens
        .into_iter()
        .map(|token| {
            let path = PathBuf::from(from_file_uri(&token).unwrap_or(token));
            (path.is_absolute() && path.is_file()).then_some(path)
        })
        .collect()
}

/// Куски, разделённые пробелами и переводами строк, с учётом кавычек. `None`
/// при незакрытой кавычке: это уже не список путей.
fn tokenize(text: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            // Кавычка открывает только кусок целиком: Windows Terminal не кавычит
            // путь без пробелов, и апостроф в `C:\O'Neil\a.pdf` — часть пути.
            (None, '"' | '\'') if current.is_empty() => quote = Some(c),
            // На Windows обратная черта — разделитель пути, а не экранирование.
            (None, '\\') if !cfg!(windows) => current.push(chars.next()?),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

/// `file:///home/a%20b.pdf` → `/home/a b.pdf`; `file:///C:/x.pdf` → `C:/x.pdf`.
fn from_file_uri(token: &str) -> Option<String> {
    let rest = token.strip_prefix("file://")?;
    // Хост (`file://localhost/…`) не нужен: путь начинается с первого `/`.
    let path = &rest[rest.find('/')?..];
    let decoded = percent_decode(path)?;
    // `/C:/x` — диск Windows, ведущая черта лишняя.
    let windows_drive = decoded.as_bytes().get(2) == Some(&b':');
    Some(match decoded.strip_prefix('/') {
        Some(drive_path) if windows_drive => drive_path.to_string(),
        _ => decoded,
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str, names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join("pooprusteek_dropped").join(test);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        dir
    }

    fn quoted(path: &std::path::Path) -> String {
        format!("\"{}\"", path.display())
    }

    #[test]
    fn a_single_path_is_a_drop() {
        let dir = scratch("single", &["a.pdf"]);
        let path = dir.join("a.pdf");
        assert_eq!(parse(&path.to_string_lossy()), Some(vec![path.clone()]));
        assert_eq!(parse(&format!("  {}\n", quoted(&path))), Some(vec![path]));
    }

    /// Windows Terminal: несколько файлов через пробел, с пробелом — в кавычках.
    #[test]
    fn several_quoted_paths_with_spaces_are_one_drop() {
        let dir = scratch("several", &["my file.pdf", "b.png"]);
        let (a, b) = (dir.join("my file.pdf"), dir.join("b.png"));
        let text = format!("{} {}", quoted(&a), b.display());
        assert_eq!(parse(&text), Some(vec![a, b]));
    }

    #[test]
    fn gnome_single_quotes_are_understood() {
        let dir = scratch("gnome", &["x y.txt"]);
        let path = dir.join("x y.txt");
        assert_eq!(parse(&format!("'{}' ", path.display())), Some(vec![path]));
    }

    #[test]
    fn a_file_uri_is_decoded() {
        let dir = scratch("uri", &["a b.pdf"]);
        let path = dir.join("a b.pdf");
        let slashed = path.to_string_lossy().replace('\\', "/");
        let uri_path = if slashed.starts_with('/') {
            slashed
        } else {
            format!("/{slashed}")
        };
        let uri = format!("file://{}", uri_path.replace(' ', "%20"));
        let parsed = parse(&uri).expect("a file URI is a drop");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_file(), "{}", parsed[0].display());
    }

    /// Обратное направление: обычный текст, несуществующий файл, папка,
    /// относительный путь и незакрытая кавычка — не перетаскивание.
    #[test]
    fn anything_but_existing_files_is_plain_text() {
        let dir = scratch("plain", &["real.pdf"]);
        let real = dir.join("real.pdf");
        assert_eq!(parse("fix the bug in main.rs"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse(&dir.join("missing.pdf").to_string_lossy()), None);
        assert_eq!(
            parse(&dir.to_string_lossy()),
            None,
            "a folder is not a file"
        );
        assert_eq!(parse("real.pdf"), None, "relative text is just a word");
        assert_eq!(parse(&format!("\"{}", real.display())), None);
        let mixed = format!("{} and more", real.display());
        assert_eq!(parse(&mixed), None, "one non-file token spoils the drop");
    }

    /// Windows Terminal кавычит только пути с пробелом: апостроф внутри
    /// некавыченного пути — его часть.
    #[test]
    fn an_apostrophe_inside_an_unquoted_path_is_part_of_it() {
        let dir = scratch("apostrophe", &["o'neil.pdf", "b c.pdf"]);
        let (a, b) = (dir.join("o'neil.pdf"), dir.join("b c.pdf"));
        let text = format!("{} {}", a.display(), quoted(&b));
        assert_eq!(parse(&text), Some(vec![a, b]));
    }

    #[test]
    fn more_than_a_hundred_paths_is_not_a_drop() {
        let dir = scratch("many", &["a.txt"]);
        let path = dir.join("a.txt").display().to_string();
        assert_eq!(parse(&vec![path; MAX_DROPPED + 1].join(" ")), None);
    }
}
