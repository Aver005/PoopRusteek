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
    // Канонизируем ближайшего существующего предка и приклеиваем остаток.
    // `..` в остатке сворачиваем сами: `atomic_write` создаст недостающие папки,
    // и `нет/../../<данные>` иначе прошёл бы мимо охраны.
    let mut tail: Vec<Component<'_>> = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            return append_lexically(crate::util::strip_verbatim(&canonical), tail.iter().rev());
        }
        // `parent` снимает и `..`, у которого нет `file_name` — раньше на нём разбор сдавался.
        match (cursor.parent(), cursor.components().next_back()) {
            (Some(parent), Some(last)) if !parent.as_os_str().is_empty() => {
                tail.push(last);
                cursor = parent;
            }
            _ => {
                return append_lexically(
                    PathBuf::new(),
                    path.components().collect::<Vec<_>>().iter(),
                );
            }
        }
    }
}

/// Дописывает компоненты, сворачивая `.` и `..` по тексту. Честно только для
/// частей, которых нет на диске: симлинком несуществующая папка быть не может.
fn append_lexically<'a>(
    mut base: PathBuf,
    parts: impl Iterator<Item = &'a Component<'a>>,
) -> PathBuf {
    for part in parts {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !base.pop() {
                    base.push(part);
                }
            }
            other => base.push(other),
        }
    }
    base
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

/// Есть ли в пути папка `.pooprusteek` (слой проекта и общая папка): регистр на Windows не важен.
fn passes_through_project_layer(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name == ".pooprusteek" || (cfg!(windows) && name.eq_ignore_ascii_case(".pooprusteek"))
    })
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

    // Проектный слой `.pooprusteek/` на любой глубине: иначе модель сама подменит `base_url`.
    // Смотрим и путь как дан, и разрешённый: ссылка и `..` не должны открывать обход.
    if passes_through_project_layer(target)
        || passes_through_project_layer(&resolve_for_compare(target))
    {
        return Err(format!(
            "Refusing to write {shown_as}: .pooprusteek/ holds the project's agent configuration, which decides where requests go. Ask the user to change it."
        ));
    }
    // Папки и отдельные файлы конфигурации агента, в том числе старые места и `--config`.
    for file in crate::config::protect::protected_files() {
        if resolve_for_compare(target) == resolve_for_compare(&file) {
            return Err(format!(
                "Refusing to write {shown_as}: it belongs to this agent's own configuration. Use the slash commands (/mcp, /providers, /secrets, /whitelist) instead."
            ));
        }
    }
    let mut guarded: Vec<PathBuf> = crate::config::protect::protected_dirs();
    guarded.extend(crate::instructions::global_dir());
    for dir in guarded {
        if is_inside(target, &dir) {
            return Err(format!(
                "Refusing to write {shown_as}: it belongs to this agent's own configuration. Use the slash commands (/mcp, /providers, /secrets, /whitelist) instead."
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
    // Путь, каким его назвали, тоже: каноническая форма ссылки не содержит `.pooprusteek`.
    refuse_protected(path, shown_as)?;
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
    fn missing_dirs_and_dotdot_cannot_smuggle_a_path_into_a_guarded_dir() {
        // Обход: `atomic_write` создаёт `missing`, и `..` уводит запись в охраняемую папку.
        let root = temp_dir("smuggle");
        let guarded = root.join("guarded");
        std::fs::create_dir_all(&guarded).unwrap();
        let smuggled = root
            .join("project")
            .join("missing")
            .join("..")
            .join("..")
            .join("guarded")
            .join("whitelist.json");
        assert!(
            is_inside(&smuggled, &guarded),
            "обход через несуществующую папку и `..`"
        );

        // И обратная сторона: из охраняемой папки `..` выводит наружу.
        let escaped = guarded
            .join("missing")
            .join("..")
            .join("..")
            .join("other.json");
        assert!(
            !is_inside(&escaped, &guarded),
            "выход через `..` засчитан как вложенность"
        );
        let _ = std::fs::remove_dir_all(&root);
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

    /// Общий конфиг и его папка закрыты для записи моделью, как и данные агента.
    #[test]
    fn the_shared_config_folder_and_data_dir_are_protected() {
        let config = crate::config::Config::global_path();
        assert!(refuse_protected(&config, "config.toml").is_err());
        let data = crate::config::Config::data_dir().join("whitelist.json");
        assert!(refuse_protected(&data, "whitelist.json").is_err());
    }

    /// Проектный слой закрыт для записи моделью на любой глубине, новый и существующий,
    /// через `..` и через ссылку; соседние файлы проекта пишутся как раньше.
    #[test]
    fn a_project_layer_cannot_be_written_by_the_model_however_the_path_is_spelled() {
        let dir = temp_dir("project-layer");
        let layer = dir.join("repo").join("pkg").join(".pooprusteek");
        let config = layer.join("config.toml");
        // Ещё не существует: создать его и означало бы подменить `base_url`.
        assert!(refuse_protected(&config, "config.toml").is_err());
        assert!(write_preserving(&config, "config.toml", b"x").is_err());
        assert!(!config.exists(), "nothing was written");
        let sneaky = dir
            .join("repo")
            .join("pkg")
            .join("src")
            .join("..")
            .join(".pooprusteek")
            .join("config.toml");
        assert!(refuse_protected(&sneaky, "config.toml").is_err());
        // Верхний уровень репозитория (поиск идёт вверх до корня git).
        let top = dir.join("repo").join(".pooprusteek").join("config.toml");
        assert!(refuse_protected(&top, "config.toml").is_err());
        // Другая сторона: обычные файлы проекта пишутся.
        let ordinary = dir.join("repo").join("pkg").join("notes.md");
        assert!(refuse_protected(&ordinary, "notes.md").is_ok());
        write_preserving(&ordinary, "notes.md", b"ok").unwrap();
        // Через ссылку: имя другое, цель внутри `.pooprusteek`.
        std::fs::create_dir_all(&layer).unwrap();
        let link = dir.join("repo").join("innocent");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&layer, &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&layer, &link).is_ok();
        if linked {
            let through = link.join("config.toml");
            assert!(refuse_protected(&through, "config.toml").is_err());
            assert!(write_preserving(&through, "config.toml", b"x").is_err());
            assert!(!config.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
