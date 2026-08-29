//! Правка файлов: точечная замена по якорю (`edit`) и запись целиком
//! (`write`). До них единственным способом менять файлы был шелл.

use super::*;
use serde_json::json;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Сколько строк каждой стороны диффа показываем модели. Дальше — счётчик.
const MAX_DIFF_LINES: usize = 20;
/// Предел ширины строки в дифф-превью, в байтах.
const MAX_DIFF_LINE_BYTES: usize = 200;

/// `edit` — заменить точный фрагмент текста в файле.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_string(),
            description:
                "Replace an exact fragment of a file. Read the file first, then copy 'old_string' from it verbatim with enough surrounding lines to be unique — it is a literal substring, not a regex. Returns a diff of the change. Use this, never a shell command, to edit a file."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, absolute or relative to the working directory"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Literal text to replace, copied from the file including its indentation"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text. Empty deletes the fragment."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring exactly one (default false). For renaming, not for short anchors."
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(path) = string_arg(&args, "path") else {
            return ToolResult::error("Missing 'path' argument");
        };
        // Пустая строка — валидная замена (удаление), поэтому здесь
        // проверяем только присутствие ключа, а не непустоту.
        let (Some(old_string), Some(new_string)) = (
            args["old_string"].as_str().map(str::to_string),
            args["new_string"].as_str().map(str::to_string),
        ) else {
            return ToolResult::error("Both 'old_string' and 'new_string' are required strings");
        };
        let replace_all = bool_arg(&args, "replace_all");

        // Файловый ввод-вывод блокирующий — уводим с асинхронного воркера
        // (инвариант 9).
        let outcome = tokio::task::spawn_blocking(move || {
            edit_blocking(&path, &old_string, &new_string, replace_all)
        })
        .await
        .unwrap_or_else(|e| Err(format!("Internal error while editing: {e}")));

        into_result(outcome)
    }
}

/// `write` — создать файл или перезаписать его целиком.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description:
                "Create a file, or replace an existing one entirely. Missing parent directories are created. It cannot append — to change part of a file that exists, use 'edit'."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, absolute or relative to the working directory"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full new content of the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(path) = string_arg(&args, "path") else {
            return ToolResult::error("Missing 'path' argument");
        };
        let Some(content) = args["content"].as_str().map(str::to_string) else {
            return ToolResult::error("Missing 'content' argument");
        };

        let outcome = tokio::task::spawn_blocking(move || write_blocking(&path, &content))
            .await
            .unwrap_or_else(|e| Err(format!("Internal error while writing: {e}")));

        into_result(outcome)
    }
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args[key]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
}

/// Флаг, присланный строкой (`"true"`), — обычная оговорка модели, и молча
/// понимать её как `false` хуже, чем принять.
fn bool_arg(args: &Value, key: &str) -> bool {
    args[key]
        .as_bool()
        .or_else(|| args[key].as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(false)
}

fn into_result(outcome: Result<String, String>) -> ToolResult {
    match outcome {
        Ok(body) => ToolResult::success(&body),
        Err(msg) => ToolResult::error(&msg),
    }
}

/// Читает файл строго как UTF-8. Именно строго: лояльное декодирование здесь
/// означает записать обратно испорченные байты, поэтому бинарник — отказ.
fn read_text(path: &Path, path_str: &str) -> Result<String, String> {
    ensure_regular_file(path, path_str)?;
    let bytes = std::fs::read(path).map_err(|e| classify_io_error(path_str, &e))?;
    String::from_utf8(bytes).map_err(|_| {
        format!("{path_str} is not valid UTF-8 text — refusing to edit it (writing back would corrupt the file)")
    })
}

/// Собственные файлы агента правит человек через команды, не модель: запись в
/// MCP-конфиг — это чужая команда, запускаемая при следующем старте.
fn refuse_protected_path(target: &Path, path_str: &str) -> Result<(), String> {
    const MCP_CONFIG_NAMES: &[&str] = &["mcp.config.json", "mcp.json"];

    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if MCP_CONFIG_NAMES.contains(&name) {
        return Err(format!(
            "Refusing to write {path_str}: an MCP config runs its servers as child processes on the next start. Ask the user to change it with /mcp."
        ));
    }

    // Канонизируем оба конца: иначе символическая ссылка или `..` в пути
    // проводят запись мимо проверки.
    let canonical = target.canonicalize();
    let resolved = canonical.as_deref().unwrap_or(target);
    for guarded in [crate::config::Config::data_dir(), owned_config_dir()] {
        let guarded = guarded.canonicalize().unwrap_or(guarded);
        if resolved.starts_with(&guarded) {
            return Err(format!(
                "Refusing to write {path_str}: it belongs to this agent's own configuration. Use the slash commands (/mcp, /providers, /whitelist) instead."
            ));
        }
    }
    Ok(())
}

fn owned_config_dir() -> PathBuf {
    crate::config::Config::path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Запись поверх существующего файла. `atomic_write` рассчитан на файлы,
/// которые приложение создаёт само, и на чужих теряет две вещи — их и
/// возвращаем: цель символической ссылки и права доступа.
fn write_back(path: &Path, path_str: &str, contents: &str) -> Result<(), String> {
    // Без канонизации `rename` кладёт обычный файл ПОВЕРХ самой ссылки:
    // ссылка исчезает, а целевой файл остаётся старым.
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    refuse_protected_path(&target, path_str)?;

    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    crate::util::atomic_write(&target, contents.as_bytes())
        .map_err(|e| format!("Failed to write {path_str}: {e}"))?;
    // Новый инод получает права по umask, а не исходные: без этого правка
    // `deploy.sh` снимает с него бит исполнения.
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(&target, permissions);
    }
    Ok(())
}

/// Размер и время правки на момент чтения. Проект штатно гоняет параллельные
/// ходы, а потерянное обновление обе стороны рапортуют как успех.
fn change_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

fn edit_blocking(
    path_str: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("'old_string' must not be empty — it would match everywhere. To create a file use the 'write' tool.".to_string());
    }
    if old_string == new_string {
        return Err(
            "'old_string' and 'new_string' are identical — that edit would change nothing."
                .to_string(),
        );
    }

    let path = crate::util::expand_tilde(path_str);
    let original = read_text(&path, path_str)?;
    let before = change_stamp(&path);

    // Файл с CRLF, а якорь пришёл с голыми LF — обычный случай на Windows,
    // и это не ошибка модели. Подгоняем перевод строки под файл.
    let (old_string, new_string) = match_line_endings(&original, old_string, new_string);
    let old_string = old_string.as_ref();
    let new_string = new_string.as_ref();

    let hits = original.matches(old_string).count();
    match hits {
        0 => {
            return Err(format!(
                "'old_string' was not found in {path_str}. Read the file again and copy the fragment verbatim, including indentation. It is matched literally, not as a pattern."
            ));
        }
        n if n > 1 && !replace_all => {
            return Err(format!(
                "'old_string' occurs {n} times in {path_str}. Add surrounding lines to make it unique, or set replace_all=true to change all {n}."
            ));
        }
        _ => {}
    }

    let updated = if replace_all {
        original.replace(old_string, new_string)
    } else {
        original.replacen(old_string, new_string, 1)
    };

    if change_stamp(&path) != before {
        return Err(format!(
            "{path_str} changed on disk while this edit was being prepared — nothing was written. Read it again and redo the edit."
        ));
    }
    write_back(&path, path_str, &updated)?;

    let plural = if hits == 1 { "" } else { "s" };
    let header = format!("Edited {path_str} ({hits} replacement{plural})");
    // Массовая замена по короткому якорю делает «удалённым» весь файл, и дифф
    // вываливает его содержимое в контекст. Для переименования он и не нужен.
    if replace_all && hits > 1 {
        return Ok(format!(
            "{header}\n  (diff omitted for a bulk replacement across {hits} sites)"
        ));
    }
    Ok(format!("{header}\n{}", render_diff(&original, &updated)))
}

fn write_blocking(path_str: &str, content: &str) -> Result<String, String> {
    let path = crate::util::expand_tilde(path_str);
    if path.is_dir() {
        return Err(format!("{path_str} is a directory, not a file"));
    }

    // Существование берём из метаданных, а не из успеха чтения: бинарный или
    // недоступный файл иначе выглядел бы как создание нового.
    let previous = std::fs::metadata(&path).ok().filter(|m| m.is_file());
    let previous_text = previous
        .is_some()
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten();

    match &previous {
        Some(_) => write_back(&path, path_str, content)?,
        None => {
            refuse_protected_path(&path, path_str)?;
            crate::util::atomic_write(&path, content.as_bytes())
                .map_err(|e| format!("Failed to write {path_str}: {e}"))?;
        }
    }

    let lines = line_count(content);
    let bytes = content.len();
    Ok(match (previous, previous_text) {
        (None, _) => format!("Created {path_str} ({lines} lines, {bytes} bytes)"),
        (Some(_), Some(old)) => format!(
            "Overwrote {path_str} ({} lines → {lines} lines, {bytes} bytes)",
            line_count(&old)
        ),
        // Файл был, но текстом не читается — сказать «создан» значит соврать
        // о потере данных.
        (Some(meta), None) => format!(
            "Overwrote {path_str} ({} bytes of non-text content → {lines} lines, {bytes} bytes)",
            meta.len()
        ),
    })
}

/// Число строк так, как их видит человек: пустой файл — ноль, файл без
/// финального перевода строки — всё равно считается целиком.
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

fn to_crlf(text: &str) -> String {
    // Через LF, а не заменой `\n` напрямую: иначе уже готовый `\r\n`
    // превращается в `\r\r\n` и файл портится.
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Приводит якорь и замену к переводу строки самого файла. Решает про файл, а
/// не про якорь: однострочный якорь с многострочной заменой иначе вставлял в
/// CRLF-файл голый LF.
fn match_line_endings<'a>(
    original: &str,
    old_string: &'a str,
    new_string: &'a str,
) -> (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>) {
    use std::borrow::Cow;
    if !original.contains("\r\n") {
        return (Cow::Borrowed(old_string), Cow::Borrowed(new_string));
    }
    // Якорь трогаем, только если как есть он не нашёлся: в файле со смешанными
    // концами строк дословное совпадение — сигнал сильнее догадки.
    let old_out = if original.contains(old_string) {
        Cow::Borrowed(old_string)
    } else {
        Cow::Owned(to_crlf(old_string))
    };
    (old_out, Cow::Owned(to_crlf(new_string)))
}

/// Дифф двух версий текста: срезаем общий префикс и суффикс построчно,
/// показываем только середину. Этого хватает, чтобы модель увидела результат
/// своей правки, и не хватает, чтобы раздуть контекст.
fn render_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let max_common = old_lines.len().min(new_lines.len());
    let prefix = (0..max_common)
        .take_while(|&i| old_lines[i] == new_lines[i])
        .count();
    let suffix = (0..max_common - prefix)
        .take_while(|&i| old_lines[old_lines.len() - 1 - i] == new_lines[new_lines.len() - 1 - i])
        .count();

    let removed = &old_lines[prefix..old_lines.len() - suffix];
    let added = &new_lines[prefix..new_lines.len() - suffix];
    if removed.is_empty() && added.is_empty() {
        // `lines()` срезает и `\n`, и `\r`, поэтому правка одних лишь концов
        // строк отсюда невидима. Промолчать — сказать модели, что правка не
        // применилась, и получить повтор.
        return "  (only line endings or the final newline changed)".to_string();
    }

    let mut out = format!("  @@ line {} @@\n", prefix + 1);
    push_diff_side(&mut out, removed, '-');
    push_diff_side(&mut out, added, '+');
    out.pop();
    out
}

fn push_diff_side(out: &mut String, lines: &[&str], marker: char) {
    for line in lines.iter().take(MAX_DIFF_LINES) {
        let _ = writeln!(
            out,
            "  {marker} {}",
            crate::util::truncate_with_ellipsis(line, MAX_DIFF_LINE_BYTES)
        );
    }
    if lines.len() > MAX_DIFF_LINES {
        let _ = writeln!(
            out,
            "  {marker} … {} more line(s)",
            lines.len() - MAX_DIFF_LINES
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог, который сам за собой убирает: без `Drop` каждый прогон
    /// оставлял по десятку папок в `%TEMP%`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pooprusteek_edit_test_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, content: &str) -> PathBuf {
            let path = self.0.join("file.txt");
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_file(tag: &str, content: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new(tag);
        let path = dir.file(content);
        (dir, path)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn edit(path: &Path, old: &str, new: &str, all: bool) -> Result<String, String> {
        edit_blocking(path.to_str().unwrap(), old, new, all)
    }

    #[test]
    fn replaces_a_unique_fragment() {
        let (_dir, path) = temp_file("unique", "let x = 1;\nlet y = 2;\n");
        let out = edit(&path, "let x = 1;", "let x = 42;", false).unwrap();
        assert_eq!(read(&path), "let x = 42;\nlet y = 2;\n");
        assert!(out.contains("1 replacement"), "{out}");
        assert!(out.contains("- let x = 1;"), "{out}");
        assert!(out.contains("+ let x = 42;"), "{out}");
    }

    #[test]
    fn ambiguous_anchor_is_refused_and_file_untouched() {
        let (_dir, path) = temp_file("ambig", "a\na\n");
        let err = edit(&path, "a", "b", false).unwrap_err();
        assert!(err.contains("occurs 2 times"), "{err}");
        assert_eq!(read(&path), "a\na\n", "file must not change on a refusal");
    }

    #[test]
    fn replace_all_changes_every_occurrence() {
        let (_dir, path) = temp_file("all", "a\na\na\n");
        let out = edit(&path, "a", "b", true).unwrap();
        assert_eq!(read(&path), "b\nb\nb\n");
        assert!(out.contains("3 replacements"), "{out}");
    }

    #[test]
    fn a_bulk_replacement_does_not_echo_the_file_into_the_diff() {
        // Короткий якорь по всему файлу иначе делает «удалённым» весь текст —
        // так утекало содержимое .env в контекст и в историю.
        let (_dir, path) = temp_file("leak", "SECRET=abc\nTOKEN=xyz\nKEY=123\n");
        let out = edit(&path, "=", "= ", true).unwrap();
        assert!(out.contains("diff omitted"), "{out}");
        assert!(!out.contains("abc"), "содержимое утекло в дифф: {out}");
        assert!(!out.contains("xyz"), "содержимое утекло в дифф: {out}");
    }

    #[test]
    fn a_single_hit_under_replace_all_still_shows_its_diff() {
        let (_dir, path) = temp_file("all_one", "alpha\nbeta\n");
        let out = edit(&path, "alpha", "ALPHA", true).unwrap();
        assert!(out.contains("+ ALPHA"), "{out}");
    }

    #[test]
    fn missing_anchor_reports_it_without_writing() {
        let (_dir, path) = temp_file("missing", "hello\n");
        let err = edit(&path, "goodbye", "hi", false).unwrap_err();
        assert!(err.contains("was not found"), "{err}");
        assert_eq!(read(&path), "hello\n");
    }

    #[test]
    fn a_missing_file_is_reported_as_not_found() {
        let dir = TempDir::new("absent");
        let path = dir.0.join("nope.txt");
        let err = edit(&path, "a", "b", false).unwrap_err();
        assert!(err.contains("File not found"), "{err}");
    }

    #[test]
    fn empty_anchor_is_refused() {
        let (_dir, path) = temp_file("empty", "hello\n");
        let err = edit(&path, "", "x", false).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn identical_strings_are_refused() {
        let (_dir, path) = temp_file("same", "hello\n");
        let err = edit(&path, "hello", "hello", false).unwrap_err();
        assert!(err.contains("identical"), "{err}");
    }

    #[test]
    fn binary_file_is_refused_rather_than_mangled() {
        let (_dir, path) = temp_file("binary", "");
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x41]).unwrap();
        let err = edit(&path, "A", "B", false).unwrap_err();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), vec![0xff, 0xfe, 0x00, 0x41]);
    }

    #[test]
    fn crlf_file_accepts_an_lf_anchor_and_stays_crlf() {
        let (_dir, path) = temp_file("crlf", "one\r\ntwo\r\nthree\r\n");
        edit(&path, "one\ntwo", "one\nTWO", false).unwrap();
        assert_eq!(read(&path), "one\r\nTWO\r\nthree\r\n");
    }

    #[test]
    fn a_crlf_replacement_into_a_crlf_file_does_not_double_the_cr() {
        // Модель может скопировать фрагмент уже с CRLF — слепая конвертация
        // давала `\r\r\n` и портила файл.
        let (_dir, path) = temp_file("crlf_new", "one\r\ntwo\r\nthree\r\n");
        edit(&path, "one\ntwo", "one\r\nTWO", false).unwrap();
        assert_eq!(read(&path), "one\r\nTWO\r\nthree\r\n");
        assert!(!read(&path).contains("\r\r"));
    }

    #[test]
    fn a_single_line_anchor_still_gets_a_crlf_replacement() {
        // Самая частая правка — одну строку на несколько; раньше подгонка
        // концов строк её пропускала и вставляла в CRLF-файл голый LF.
        let (_dir, path) = temp_file("crlf_single", "one\r\ntwo\r\nthree\r\n");
        edit(&path, "two", "two\nTWOandahalf", false).unwrap();
        assert_eq!(read(&path), "one\r\ntwo\r\nTWOandahalf\r\nthree\r\n");
    }

    #[test]
    fn an_lf_file_is_left_on_lf() {
        let (_dir, path) = temp_file("lf_stays", "one\ntwo\n");
        edit(&path, "two", "TWO", false).unwrap();
        assert_eq!(read(&path), "one\nTWO\n");
        assert!(!read(&path).contains('\r'));
    }

    #[test]
    fn deleting_a_fragment_works() {
        let (_dir, path) = temp_file("delete", "keep\nremove\nkeep2\n");
        edit(&path, "remove\n", "", false).unwrap();
        assert_eq!(read(&path), "keep\nkeep2\n");
    }

    #[test]
    fn multibyte_content_survives_a_round_trip() {
        let (_dir, path) = temp_file("utf8", "привет мир\nдруг 🐙\n");
        edit(&path, "мир", "вселенная", false).unwrap();
        assert_eq!(read(&path), "привет вселенная\nдруг 🐙\n");
    }

    #[test]
    fn dropping_the_final_newline_is_reported_not_called_a_no_op() {
        let (_dir, path) = temp_file("final_nl", "a\nb\n");
        let out = edit(&path, "b\n", "b", false).unwrap();
        assert_eq!(read(&path), "a\nb");
        assert!(out.contains("line endings or the final newline"), "{out}");
    }

    #[test]
    fn a_symlink_is_followed_instead_of_being_replaced() {
        let dir = TempDir::new("symlink");
        let real = dir.0.join("real.txt");
        std::fs::write(&real, "secret\n").unwrap();
        let link = dir.0.join("link.txt");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&real, &link).is_ok();
        if !made {
            // Windows требует прав на создание ссылок; без них проверять нечего.
            return;
        }
        edit(&link, "secret", "changed", false).unwrap();
        assert_eq!(
            read(&real),
            "changed\n",
            "правка не дошла до целевого файла"
        );
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "символическая ссылка уничтожена"
        );
    }

    #[test]
    fn a_concurrent_change_is_noticed_by_the_stamp() {
        let (_dir, path) = temp_file("race", "original\n");
        let before = change_stamp(&path);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "someone else wrote this\n").unwrap();
        assert_ne!(change_stamp(&path), before, "штамп не заметил чужую запись");
    }

    #[test]
    fn refuses_to_write_an_mcp_config() {
        let dir = TempDir::new("mcp");
        let path = dir.0.join("mcp.config.json");
        let err = write_blocking(path.to_str().unwrap(), "{}").unwrap_err();
        assert!(err.contains("MCP config"), "{err}");
        assert!(!path.exists(), "файл всё-таки создан");
    }

    #[test]
    fn refuses_to_write_into_the_agents_own_data_dir() {
        let target = crate::config::Config::data_dir().join("whitelist.json");
        let err = write_blocking(target.to_str().unwrap(), "[]").unwrap_err();
        assert!(err.contains("own configuration"), "{err}");
    }

    #[test]
    fn write_creates_a_file_and_its_parents() {
        let dir = TempDir::new("write_new");
        let path = dir.0.join("nested").join("new.txt");
        let out = write_blocking(path.to_str().unwrap(), "hello\nworld\n").unwrap();
        assert_eq!(read(&path), "hello\nworld\n");
        assert!(out.contains("Created"), "{out}");
        assert!(out.contains("2 lines"), "{out}");
    }

    #[test]
    fn write_reports_an_overwrite_with_both_sizes() {
        let (_dir, path) = temp_file("overwrite", "a\nb\nc\n");
        let out = write_blocking(path.to_str().unwrap(), "x\n").unwrap();
        assert_eq!(read(&path), "x\n");
        assert!(out.contains("Overwrote"), "{out}");
        assert!(out.contains("3 lines → 1 lines"), "{out}");
    }

    #[test]
    fn overwriting_a_binary_file_is_not_called_a_creation() {
        let (_dir, path) = temp_file("write_binary", "");
        std::fs::write(&path, [0xffu8, 0xfe, 0x41]).unwrap();
        let out = write_blocking(path.to_str().unwrap(), "hello\n").unwrap();
        assert!(out.contains("Overwrote"), "{out}");
        assert!(out.contains("non-text"), "{out}");
    }

    #[test]
    fn write_to_a_directory_is_refused() {
        let dir = TempDir::new("write_dir");
        let err = write_blocking(dir.0.to_str().unwrap(), "x").unwrap_err();
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn write_of_empty_content_is_allowed_and_counted_as_zero_lines() {
        let dir = TempDir::new("write_empty");
        let path = dir.0.join("blank.txt");
        let out = write_blocking(path.to_str().unwrap(), "").unwrap();
        assert!(out.contains("0 lines"), "{out}");
        assert_eq!(read(&path), "");
    }

    #[test]
    fn editing_a_directory_is_refused() {
        let (dir, _path) = temp_file("dir", "x");
        let err = edit(&dir.0, "x", "y", false).unwrap_err();
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn line_count_matches_what_a_human_would_say() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\nb\n"), 2);
    }

    #[test]
    fn a_bool_flag_sent_as_a_string_is_honoured() {
        assert!(bool_arg(&json!({"replace_all": "true"}), "replace_all"));
        assert!(bool_arg(&json!({"replace_all": true}), "replace_all"));
        assert!(!bool_arg(&json!({}), "replace_all"));
        assert!(!bool_arg(
            &json!({"replace_all": "nonsense"}),
            "replace_all"
        ));
    }

    #[tokio::test]
    async fn missing_arguments_are_reported_rather_than_panicking() {
        let blank = EditTool.execute(json!({"path": "   "})).await;
        assert!(blank.is_error && blank.content.contains("Missing 'path'"));
        let no_strings = EditTool.execute(json!({"path": "x.txt"})).await;
        assert!(no_strings.is_error && no_strings.content.contains("required strings"));
        let no_content = WriteTool.execute(json!({"path": "x.txt"})).await;
        assert!(no_content.is_error && no_content.content.contains("Missing 'content'"));
    }

    #[test]
    fn diff_caps_long_edits_and_counts_the_rest() {
        let old: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..50).map(|i| format!("changed {i}\n")).collect();
        let diff = render_diff(&old, &new);
        assert!(diff.contains("30 more line(s)"), "{diff}");
    }

    #[test]
    fn diff_points_at_the_first_changed_line() {
        let diff = render_diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(diff.contains("@@ line 2 @@"), "{diff}");
        assert!(diff.contains("- b"), "{diff}");
        assert!(diff.contains("+ B"), "{diff}");
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        // Инвариант 4: срез по байтам порвал бы двухбайтный символ.
        let long = "я".repeat(MAX_DIFF_LINE_BYTES);
        let diff = render_diff("a\n", &format!("{long}\n"));
        assert!(diff.contains('я'), "{diff}");
    }
}
