//! Где что лежит. Открытый конфиг — `~/.pooprusteek/config.toml`, данные и кэш —
//! в локальной (не roaming) папке. Старые места читаются при переносе.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Файл, заданный флагом `--config`: запись идёт только в него.
static EXPLICIT_CONFIG: OnceLock<PathBuf> = OnceLock::new();
/// Папка данных, заданная флагом `--data-dir`.
pub(super) static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();
/// Папка данных, выбранная при старте после (не)удавшегося переноса.
static STARTUP_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub const DATA_MARKER: &str = ".migrated-from-legacy";

pub fn set_explicit_config(path: &Path) {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let _ = EXPLICIT_CONFIG.set(absolute);
}

pub fn explicit_config() -> Option<&'static Path> {
    EXPLICIT_CONFIG.get().map(PathBuf::as_path)
}

/// Прогон с `--config` или `--data-dir`: настоящее хранилище и настоящие папки не трогаем.
pub fn is_isolated() -> bool {
    EXPLICIT_CONFIG.get().is_some() || DATA_DIR_OVERRIDE.get().is_some()
}

/// `~/.pooprusteek`: общий слой и папка глобальных правил (`AGENTS.md`).
pub fn global_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".pooprusteek"))
        .or_else(|| dirs::config_dir().map(|dir| dir.join("pooprusteek")))
        .unwrap_or_else(|| PathBuf::from(".pooprusteek"))
}

pub fn global_config_path() -> PathBuf {
    global_dir().join("config.toml")
}

/// Старый конфиг (`dirs::config_dir()/pooprusteek/config.toml`).
pub fn legacy_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("pooprusteek").join("config.toml"))
}

/// Новая папка данных и кэша.
pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pooprusteek")
}

/// Старая папка данных (`dirs::data_dir()/pooprusteek`).
pub fn legacy_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("pooprusteek"))
}

/// Папка данных процесса. Выбирается один раз при старте (`choose_startup_data_dir`),
/// без этого — новая; сама по себе ничего на диске не проверяет и не переносит.
pub fn effective_data_dir() -> PathBuf {
    STARTUP_DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(default_data_dir)
}

pub fn set_startup_data_dir(path: PathBuf) {
    let _ = STARTUP_DATA_DIR.set(path);
}

/// Где пишется конфиг: файл из `--config` или общий.
pub fn write_target() -> PathBuf {
    // Тест, который случайно сохранит конфиг, не должен задеть настоящий.
    #[cfg(test)]
    {
        std::env::temp_dir()
            .join("pooprusteek-unit-tests")
            .join("config.toml")
    }
    #[cfg(not(test))]
    {
        explicit_config()
            .map(Path::to_path_buf)
            .unwrap_or_else(global_config_path)
    }
}

/// Корень проекта для слоя `.pooprusteek/`: вверх от `start` до корня git;
/// вне git — только сама папка запуска. Возвращает папку с `.pooprusteek/config.toml`.
pub fn find_project_layer(start: &Path) -> Option<PathBuf> {
    let global = global_dir();
    let mut cursor = Some(start);
    while let Some(dir) = cursor {
        let candidate = dir.join(".pooprusteek");
        if candidate != global && candidate.join("config.toml").is_file() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            return None;
        }
        // Вне git смотрим только папку запуска.
        if !inside_git(start) {
            return None;
        }
        cursor = dir.parent();
    }
    None
}

fn inside_git(start: &Path) -> bool {
    start.ancestors().any(|dir| dir.join(".git").exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-paths-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn layer_in(dir: &Path) {
        std::fs::create_dir_all(dir.join(".pooprusteek")).unwrap();
        std::fs::write(dir.join(".pooprusteek").join("config.toml"), "").unwrap();
    }

    #[test]
    fn inside_git_the_search_climbs_to_the_repository_root() {
        let root = temp("git");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        layer_in(&root);
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_project_layer(&deep), Some(root.join(".pooprusteek")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_search_stops_at_the_git_root() {
        let outer = temp("outer");
        layer_in(&outer);
        let repo = outer.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            find_project_layer(&deep),
            None,
            "the layer above the repo is foreign"
        );
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn outside_git_only_the_launch_folder_counts() {
        let root = temp("nogit");
        layer_in(&root);
        let deep = root.join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_project_layer(&deep), None);
        assert_eq!(find_project_layer(&root), Some(root.join(".pooprusteek")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_nearest_layer_wins() {
        let root = temp("near");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        layer_in(&root);
        let inner = root.join("pkg");
        std::fs::create_dir_all(&inner).unwrap();
        layer_in(&inner);
        assert_eq!(find_project_layer(&inner), Some(inner.join(".pooprusteek")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
