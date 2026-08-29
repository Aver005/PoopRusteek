//! Запись файла от имени модели: что писать запрещено и как писать, ничего
//! не сломав.
//!
//! Живёт отдельным модулем, потому что путей записи два — инструменты
//! `edit`/`write` и откат чекпоинтов, — и они уже однажды разошлись: запись
//! берегла символические ссылки и права, а откат их уничтожал.

use std::path::{Component, Path, PathBuf};

/// Приводит путь к сравнимому виду, даже если его ещё нет на диске.
///
/// Просто `canonicalize` тут не годится: у несуществующей цели он падает, и
/// сравнение уходит на сырой путь. На Windows это `C:\…` против `\\?\C:\…` —
/// то есть охрана папки данных молча переставала работать для файлов, которых
/// пока нет (а именно такие модель и создаёт).
pub fn resolve_for_compare(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return crate::util::strip_verbatim(&canonical);
    }
    // Канонизируем ближайшего существующего предка и приклеиваем остаток —
    // так `..` сворачивается, а префикс совпадает с охраняемой стороной.
    let mut tail: Vec<Component<'_>> = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            let mut out = crate::util::strip_verbatim(&canonical);
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cursor.parent(), cursor.file_name()) {
            (Some(parent), Some(_)) => {
                if let Some(last) = cursor.components().next_back() {
                    tail.push(last);
                }
                cursor = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Лежит ли `path` внутри `dir`. По компонентам, а не по строке: иначе
/// `guarded-not-really.txt` считался бы лежащим внутри `guarded`. На Windows
/// сравнение регистронезависимое — `C:\Users` и `c:\users` одна папка.
fn is_inside(path: &Path, dir: &Path) -> bool {
    let parts = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let text = c.as_os_str().to_string_lossy().into_owned();
                if cfg!(windows) {
                    text.to_lowercase()
                } else {
                    text
                }
            })
            .collect()
    };
    let path = parts(&resolve_for_compare(path));
    let dir = parts(&resolve_for_compare(dir));
    path.len() > dir.len() && path[..dir.len()] == dir[..]
}

/// Пути, которые модель не правит ни инструментом, ни откатом: собственная
/// конфигурация агента и MCP-конфиги (последние исполняются как команды при
/// следующем старте).
pub fn refuse_protected(target: &Path, shown_as: &str) -> Result<(), String> {
    const MCP_CONFIG_NAMES: &[&str] = &["mcp.config.json", "mcp.json"];

    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if MCP_CONFIG_NAMES.contains(&name) {
        return Err(format!(
            "Refusing to write {shown_as}: an MCP config runs its servers as child processes on the next start. Ask the user to change it with /mcp."
        ));
    }

    let guarded = [
        Some(crate::config::Config::data_dir()),
        crate::config::Config::path()
            .parent()
            .map(Path::to_path_buf),
        crate::instructions::global_dir(),
    ];
    for dir in guarded.into_iter().flatten() {
        if is_inside(target, &dir) {
            return Err(format!(
                "Refusing to write {shown_as}: it belongs to this agent's own configuration. Use the slash commands (/mcp, /providers, /whitelist) instead."
            ));
        }
    }
    Ok(())
}

/// Запись поверх файла пользователя. `atomic_write` рассчитан на файлы,
/// которые приложение создаёт само, и на чужих теряет две вещи — их и
/// возвращаем: цель символической ссылки и права доступа.
pub fn write_preserving(path: &Path, shown_as: &str, contents: &[u8]) -> Result<(), String> {
    // Без канонизации `rename` кладёт обычный файл ПОВЕРХ самой ссылки:
    // ссылка исчезает, а целевой файл остаётся старым.
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    refuse_protected(&target, shown_as)?;

    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    // Файл только для чтения `rename` не примет: снимаем флаг на время записи
    // и возвращаем вместе с остальными правами.
    if let Some(permissions) = &permissions
        && permissions.readonly()
    {
        let mut writable = permissions.clone();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        let _ = std::fs::set_permissions(&target, writable);
    }

    crate::util::atomic_write(&target, contents)
        .map_err(|e| format!("Failed to write {shown_as}: {e}"))?;
    // Новый инод получает права по umask, а не исходные: без этого правка
    // `deploy.sh` снимает с него бит исполнения.
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(&target, permissions);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek_safe_write_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_that_does_not_exist_yet_still_resolves_under_its_parent() {
        // Тот самый дефект: у несуществующей цели `canonicalize` падал, путь
        // оставался сырым, и на Windows охрана папки не срабатывала.
        let dir = temp_dir("absent");
        let missing = dir.join("not-created-yet.json");
        assert!(is_inside(&missing, &dir), "несуществующий файл вне папки");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dotdot_path_cannot_step_out_and_back_in() {
        let dir = temp_dir("dotdot");
        let sneaky = dir.join("sub").join("..").join("target.json");
        assert!(is_inside(&sneaky, &dir), "`..` пронесло путь мимо охраны");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sibling_directory_is_not_inside() {
        let root = temp_dir("sibling");
        let guarded = root.join("guarded");
        std::fs::create_dir_all(&guarded).unwrap();
        assert!(!is_inside(&root.join("other").join("x.txt"), &guarded));
        // Имя-префикс не должно засчитываться за вложенность.
        assert!(!is_inside(&root.join("guarded-not-really.txt"), &guarded));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_agents_own_data_dir_is_refused_even_for_a_new_file() {
        let target = crate::config::Config::data_dir().join("brand-new-file.json");
        let error = refuse_protected(&target, "x").unwrap_err();
        assert!(error.contains("own configuration"), "{error}");
    }

    #[test]
    fn an_mcp_config_is_refused_anywhere() {
        let dir = temp_dir("mcp");
        let error = refuse_protected(&dir.join("mcp.config.json"), "x").unwrap_err();
        assert!(error.contains("MCP config"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ordinary_project_file_is_allowed() {
        let dir = temp_dir("ok");
        assert!(refuse_protected(&dir.join("src").join("main.rs"), "x").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_follows_a_symlink_instead_of_replacing_it() {
        let dir = temp_dir("symlink");
        let real = dir.join("real.txt");
        std::fs::write(&real, "old\n").unwrap();
        let link = dir.join("link.txt");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&real, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&real, &link).is_ok();
        if made {
            write_preserving(&link, "link.txt", b"new\n").unwrap();
            assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
            assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_readonly_file_can_still_be_written_and_stays_readonly() {
        // Откат такого файла раньше падал с «Отказано в доступе».
        let dir = temp_dir("readonly");
        let path = dir.join("locked.txt");
        std::fs::write(&path, "old\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        write_preserving(&path, "locked.txt", b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(
            std::fs::metadata(&path).unwrap().permissions().readonly(),
            "флаг только-для-чтения не возвращён"
        );

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
