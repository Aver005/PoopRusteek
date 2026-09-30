//! Файлы к сообщению — один путь для `/attach`, `@`, перетаскивания и `exec --attach`.
//! Модель получает полные пути и содержимое, чат — только имена.

use crate::provider::{AttachedFile, ChatMessage};
use std::path::{Path, PathBuf};

/// Больше стольких имён чат не перечисляет, а показывает счётчик.
pub const MAX_LISTED_NAMES: usize = 5;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg"];

/// Файл по пути, введённому человеком. Относительный путь — от рабочей
/// папки, как у `@file`; `./` убирается, чтобы один файл не прикрепился дважды.
pub fn resolve(raw: &str, workspace: &Path) -> Result<AttachedFile, String> {
    let path = Path::new(raw);
    let resolved: PathBuf = if path.is_relative() {
        workspace.join(path)
    } else {
        path.to_path_buf()
    }
    .components()
    .collect();
    let metadata = std::fs::metadata(&resolved).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("File not found: {raw}"),
        _ => format!("Cannot read {raw}: {e}"),
    })?;
    if !metadata.is_file() {
        return Err(format!("Not a file: {raw}"));
    }
    let display_name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw)
        .to_string();
    let is_image = resolved
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
    Ok(AttachedFile {
        display_name,
        path: resolved.to_string_lossy().into_owned(),
        size: metadata.len(),
        is_image,
    })
}

/// Больше не встраиваем текстом: читается на главном цикле и забивает контекст.
pub const MAX_INLINE_TEXT_BYTES: u64 = 1024 * 1024;

/// Текст файла, если его можно вставить в сообщение: текстовый (UTF-8 или
/// UTF-16 с BOM) и не больше [`MAX_INLINE_TEXT_BYTES`]. `None` — прикладывать файлом.
pub fn inline_text(path: &Path) -> std::io::Result<Option<String>> {
    if std::fs::metadata(path)?.len() > MAX_INLINE_TEXT_BYTES
        || !crate::util::looks_like_text(path)?
    {
        return Ok(None);
    }
    match crate::util::read_text(path) {
        Ok(content) => Ok(Some(content)),
        // Не-UTF-8 дальше окна проверки — тоже файл, а не «не прочитать».
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Ok(None),
        Err(e) => Err(e),
    }
}

/// `@`-упоминания, которые не встраиваются текстом (двоичные, большие): их
/// прикладываем файлом, а `@report.pdf` раньше молча оставался словом.
pub fn binary_mentions(input: &str, workspace: &Path) -> Vec<AttachedFile> {
    crate::cli::file_mentions::mention_targets(input, workspace)
        .filter(|target| !crate::cli::file_mentions::expands_inline(target))
        .filter_map(|target| resolve(&target.path.to_string_lossy(), workspace).ok())
        .collect()
}

/// Что модель получает о файле текстом, и нужно ли провайдеру приложить его.
enum Part {
    Text(String),
    Upload(String),
}

fn part_for(file: &AttachedFile, accepts: &dyn Fn(&Path) -> bool) -> Part {
    let path = Path::new(&file.path);
    let shown = &file.path;
    let name = &file.display_name;
    match inline_text(path) {
        Err(e) => Part::Text(format!("Вложение: {shown} — не удалось прочитать: {e}")),
        Ok(Some(content)) => Part::Text(crate::provider::prompt::attached_file(shown, &content)),
        Ok(None) if accepts(path) => Part::Upload(format!(
            "Вложение: {shown} — файл «{name}» приложен к сообщению."
        )),
        Ok(None) => Part::Text(format!(
            "Вложение: {shown} — файл «{name}» (двоичный или больше 1 МБ): его содержимого в сообщении нет, только путь."
        )),
    }
}

/// Добавить файл, если такого пути ещё нет.
pub fn push_unique(files: &mut Vec<AttachedFile>, file: AttachedFile) {
    if !files.iter().any(|f| f.path == file.path) {
        files.push(file);
    }
}

/// Сообщение с файлами: модели — `text`, полные пути и содержимое; чату —
/// `typed` и сводка по `named` (файлы, которых в тексте не видно, — не чипы).
pub fn user_message(
    text: &str,
    typed: &str,
    files: &[AttachedFile],
    named: &[AttachedFile],
    accepts: &dyn Fn(&Path) -> bool,
) -> ChatMessage {
    if files.is_empty() {
        return ChatMessage::user(text);
    }
    let mut blocks = Vec::with_capacity(files.len());
    let mut uploads = Vec::new();
    for file in files {
        match part_for(file, accepts) {
            Part::Text(text) => blocks.push(text),
            Part::Upload(note) => {
                blocks.push(note);
                uploads.push(file.path.clone());
            }
        }
    }
    let content = if text.trim().is_empty() {
        blocks.join("\n\n")
    } else {
        format!("{text}\n\n{}", blocks.join("\n\n"))
    };
    let names: Vec<&str> = named.iter().map(|f| f.display_name.as_str()).collect();
    let display = if names.is_empty() {
        typed.to_string()
    } else {
        format!("{typed}\n📎 {}", summary(&names))
    };
    ChatMessage {
        attachments: uploads,
        ..ChatMessage::user_with_display(&content, display.trim_start())
    }
}

/// Имена через запятую, а больше [`MAX_LISTED_NAMES`] — просто счётчик.
pub fn summary(names: &[&str]) -> String {
    if names.len() > MAX_LISTED_NAMES {
        format!("{} files", names.len())
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("pooprusteek_attachments")
            .join(test);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Сообщение с одним файлом, прикреплённым через `/attach` (сводка его называет).
    fn send(text: &str, file: &AttachedFile, accepts: &dyn Fn(&Path) -> bool) -> ChatMessage {
        let one = std::slice::from_ref(file);
        user_message(text, text, one, one, accepts)
    }

    fn file(dir: &Path, name: &str, bytes: &[u8]) -> AttachedFile {
        std::fs::write(dir.join(name), bytes).unwrap();
        resolve(name, dir).unwrap()
    }

    #[test]
    fn a_relative_path_resolves_against_the_workspace() {
        let dir = scratch("relative");
        let f = file(&dir, "notes.md", b"hi");
        assert_eq!(Path::new(&f.path), dir.join("notes.md"));
        assert_eq!(f.display_name, "notes.md");
        assert!(
            resolve("missing.md", &dir)
                .unwrap_err()
                .contains("not found")
        );
        assert!(resolve(".", &dir).unwrap_err().contains("Not a file"));
    }

    /// Текст — в рамке вложения с полным путём; чат видит только имя.
    #[test]
    fn a_text_file_goes_inline_with_its_full_path() {
        let dir = scratch("text");
        let f = file(&dir, "notes.md", b"remember the milk");
        let msg = send("look", &f, &|_| true);
        assert!(msg.content.starts_with("look\n\n"));
        assert!(msg.content.contains(&format!("[file name]: {}\n", f.path)));
        assert!(msg.content.contains("remember the milk"));
        assert!(msg.attachments.is_empty(), "text needs no upload");
        assert_eq!(msg.visible_content(), "look\n📎 notes.md");
    }

    /// Двоичный файл уходит вложением к провайдеру, который умеет, а модель
    /// всё равно знает полный путь.
    #[test]
    fn a_binary_file_is_uploaded_when_the_provider_accepts_it() {
        let dir = scratch("binary");
        let f = file(&dir, "scan.pdf", b"%PDF-1.7\n\x00\x01\x02");
        let msg = send("", &f, &|_| true);
        assert_eq!(msg.attachments, vec![f.path.clone()]);
        assert!(msg.content.contains(&f.path));
        assert!(!msg.content.contains("%PDF"), "bytes never go inline");
        assert_eq!(msg.visible_content(), "📎 scan.pdf");
    }

    #[test]
    fn a_binary_file_is_only_named_when_the_provider_cannot_take_it() {
        let dir = scratch("binary_refused");
        let f = file(&dir, "photo.png", b"\x89PNG\r\n\x1a\n\x00");
        let msg = send("what is this", &f, &|_| false);
        assert!(msg.attachments.is_empty());
        assert!(msg.content.contains(&f.path));
        assert!(msg.content.contains("только путь"), "{}", msg.content);
    }

    /// Файл пропал между прикреплением и отправкой: он не теряется молча, и
    /// модель узнаёт причину, а не «двоичное содержимое».
    #[test]
    fn a_file_gone_before_sending_is_reported_not_dropped() {
        let dir = scratch("gone");
        let f = file(&dir, "gone.png", b"x");
        std::fs::remove_file(&f.path).unwrap();
        let msg = send("", &f, &|_| true);
        assert!(
            msg.content.contains("не удалось прочитать"),
            "{}",
            msg.content
        );
        assert!(msg.attachments.is_empty(), "nothing to upload");
        assert_eq!(msg.visible_content(), "📎 gone.png");
    }

    /// Обрезанный на границе окна многобайтовый символ — не повод считать
    /// кириллицу двоичной; UTF-16 с BOM — тоже текст, и он декодируется.
    #[test]
    fn cyrillic_and_utf16_text_go_inline() {
        let dir = scratch("encodings");
        let ru = file(&dir, "ru.txt", "я".repeat(8 * 1024).as_bytes());
        let msg = send("", &ru, &|_| true);
        assert!(msg.attachments.is_empty() && msg.content.contains("яяя"));

        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("привет".encode_utf16().flat_map(u16::to_le_bytes));
        let f = file(&dir, "win.txt", &utf16);
        let msg = send("", &f, &|_| true);
        assert!(msg.content.contains("привет"), "{}", msg.content);
    }

    /// Текст, который ломается после окна проверки, — двоичный файл, а не
    /// «не удалось прочитать»: провайдер, что берёт `.txt`, разберёт кодировку сам.
    #[test]
    fn text_broken_past_the_sniff_window_is_uploaded() {
        let dir = scratch("late_binary");
        let mut bytes = "a".repeat(9 * 1024).into_bytes();
        bytes.extend([0xC0, 0xC1, 0xE0]); // cp1251-хвост
        let f = file(&dir, "log.txt", &bytes);
        let msg = send("", &f, &|_| true);
        assert_eq!(msg.attachments, vec![f.path]);
    }

    /// Провайдер решает по типу: архив ему не нужен — только путь.
    #[test]
    fn the_provider_decides_which_binaries_it_takes() {
        let dir = scratch("by_type");
        let zip = file(&dir, "build.zip", b"PK\x03\x04\x00");
        let msg = send("", &zip, &|p| p.extension().is_some_and(|e| e == "pdf"));
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn a_dot_segment_does_not_make_a_second_file() {
        let dir = scratch("dot");
        let plain = file(&dir, "a.pdf", b"x");
        assert_eq!(resolve("./a.pdf", &dir).unwrap().path, plain.path);
    }

    /// `@scan.pdf` в тексте — вложение; `@notes.md` — нет, его разворачивает
    /// `expand_file_mentions`.
    #[test]
    fn only_binary_mentions_become_attachments() {
        let dir = scratch("mentions");
        file(&dir, "scan.pdf", b"%PDF\x00");
        file(&dir, "notes.md", b"text");
        let found = binary_mentions("see @scan.pdf and @notes.md and @missing.png", &dir);
        let names: Vec<&str> = found.iter().map(|f| f.display_name.as_str()).collect();
        assert_eq!(names, vec!["scan.pdf"]);
    }

    /// Файл из чипа уже виден в тексте (`[📎 a.pdf]`), и сводка его не повторяет.
    #[test]
    fn a_chip_file_is_not_repeated_in_the_summary() {
        let dir = scratch("chip_summary");
        let chip = file(&dir, "a.pdf", b"%PDF\x00");
        let bar = file(&dir, "b.pdf", b"%PDF\x00");
        let typed = "see [📎 a.pdf]";
        let msg = user_message(typed, typed, &[chip, bar.clone()], &[bar], &|_| true);
        assert_eq!(
            msg.visible_content(),
            "see [📎 a.pdf]
📎 b.pdf"
        );
        assert_eq!(msg.attachments.len(), 2, "both still go to the model");
    }

    #[test]
    fn more_than_five_names_collapse_to_a_count() {
        let five = ["a", "b", "c", "d", "e"];
        assert_eq!(summary(&five), "a, b, c, d, e");
        let six = ["a", "b", "c", "d", "e", "f"];
        assert_eq!(summary(&six), "6 files");
    }
}
