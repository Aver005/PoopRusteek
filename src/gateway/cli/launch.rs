//! Описание запуска CLI: программа, argv, окружение, папки. Чистые данные, их проверяют тесты.
//! Значения окружения не печатаются: в `Debug` только имена.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Всё, что нужно процессу: ни одного скрытого входа сверх этого.
#[derive(Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Сколько первых аргументов — префикс подмены (`mock-cli`, …): охранные флаги идут после него.
    pub prefix_len: usize,
    /// Полное окружение ребёнка (родительское уже отфильтровано).
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    /// Промпт: идёт в stdin, а не в argv (предел командной строки Windows 32 К).
    pub stdin: Vec<u8>,
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launch")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("prefix_len", &self.prefix_len)
            .field("env_names", &self.env_names())
            .field("cwd", &self.cwd)
            .field("stdin_bytes", &self.stdin.len())
            .finish()
    }
}

impl Launch {
    /// Аргументы без префикса подмены: то, что собрал диалект.
    pub fn own_args(&self) -> &[OsString] {
        &self.args[self.prefix_len.min(self.args.len())..]
    }

    pub fn env_names(&self) -> Vec<String> {
        self.env
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    pub fn env_value(&self, name: &str) -> Option<&OsStr> {
        self.env
            .iter()
            .find(|(key, _)| same_name(key, name))
            .map(|(_, value)| value.as_os_str())
    }

    pub fn has_arg(&self, arg: &str) -> bool {
        self.args.iter().any(|a| a == arg)
    }

    /// Значение флага: аргумент сразу после `flag`.
    pub fn arg_after(&self, flag: &str) -> Option<&OsStr> {
        let at = self.args.iter().position(|a| a == flag)?;
        self.args.get(at + 1).map(OsString::as_os_str)
    }
}

/// Имена переменных на Windows не различают регистр.
fn same_name(key: &OsStr, name: &str) -> bool {
    let key = key.to_string_lossy();
    if cfg!(windows) {
        key.eq_ignore_ascii_case(name)
    } else {
        key == name
    }
}

/// Окружение ребёнка: родительское без `strip`, затем `set` поверх.
pub fn child_env(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    strip: impl Fn(&str) -> bool,
    set: Vec<(&str, OsString)>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !strip(&name)
                && !set
                    .iter()
                    .any(|(key, _)| same_name(OsStr::new(&*name), key))
        })
        .collect();
    env.extend(set.into_iter().map(|(name, value)| (name.into(), value)));
    env
}

/// Имя начинается с префикса без учёта регистра (`claude_`, `CLAUDE_`).
pub fn has_prefix(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Временные папки одного вызова. Падает вместе с вызовом: чужие файлы CLI там не остаются.
pub struct Scratch {
    root: PathBuf,
    keep: bool,
}

impl Scratch {
    pub fn create(base: &Path) -> std::io::Result<Self> {
        private_dir(base)?;
        let root = base.join(uuid::Uuid::new_v4().simple().to_string());
        private_dir(&root.join("work"))?;
        private_dir(&root.join("config"))?;
        Ok(Self { root, keep: false })
    }

    /// Не удалять папки при падении: проверка приманки смотрит в них после вызова.
    pub fn keep(&mut self) {
        self.keep = true;
    }

    /// Пустая рабочая папка: проекта модель не видит.
    pub fn work_dir(&self) -> PathBuf {
        self.root.join("work")
    }

    /// Пустая папка конфигурации (`XDG_CONFIG_HOME` у OpenCode).
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// Системный промпт: файл рядом с рабочей папкой, не в ней.
    pub fn write_system(&self, text: &str) -> std::io::Result<PathBuf> {
        let path = self.root.join("system.md");
        write_private(&path, text)?;
        Ok(path)
    }
}

/// Папка только для владельца (0700 на unix): чужие локальные пользователи в неё не зайдут.
fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        // Папка могла существовать с чужими правами: приводим явно.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Файл только для владельца (0600 на unix); папка вызова своя, гонки за имя нет.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        // Процесс уже убит; занятая папка на Windows не страшна, уберёт следующий вызов.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Путь для JSON и `{file:…}`: прямые слэши, чтобы разбор не съел обратные.
pub fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (name.into(), value.into())
    }

    #[test]
    fn the_parent_environment_is_filtered_and_overridden_by_name() {
        let parent = vec![
            pair("PATH", "p"),
            pair("CLAUDE_CODE_SESSION_ID", "s"),
            pair("claude_lower", "l"),
            pair("KEEP", "k"),
        ];
        let env = child_env(
            parent,
            |name| has_prefix(name, "CLAUDE_"),
            vec![("CLAUDE_CONFIG_DIR", "home".into())],
        );
        let names: Vec<String> = env
            .iter()
            .map(|(n, _)| n.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "KEEP", "CLAUDE_CONFIG_DIR"]);
    }

    #[test]
    fn a_set_value_replaces_the_parent_one() {
        let env = child_env(
            vec![pair("XDG_DATA_HOME", "old")],
            |_| false,
            vec![("XDG_DATA_HOME", "new".into())],
        );
        assert_eq!(env, vec![pair("XDG_DATA_HOME", "new")]);
    }

    #[test]
    fn debug_never_shows_environment_values() {
        let launch = Launch {
            program: "x".into(),
            args: vec![],
            prefix_len: 0,
            env: vec![pair("SECRET_TOKEN", "hunter2")],
            cwd: ".".into(),
            stdin: b"private prompt".to_vec(),
        };
        let shown = format!("{launch:?}");
        assert!(shown.contains("SECRET_TOKEN"));
        assert!(!shown.contains("hunter2") && !shown.contains("private prompt"));
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_the_folders_are_0700_and_the_prompt_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let base =
            std::env::temp_dir().join(format!("pooprusteek-scratch-modes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Папка уже была, и с открытыми правами: приводится к 0700.
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let scratch = Scratch::create(&base).unwrap();
        let system = scratch.write_system("hi").unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&base), 0o700);
        assert_eq!(mode(system.parent().unwrap()), 0o700);
        assert_eq!(mode(&scratch.work_dir()), 0o700);
        assert_eq!(mode(&scratch.config_dir()), 0o700);
        assert_eq!(mode(&system), 0o600);
        drop(scratch);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_prompt_file_is_rewritten_in_place_and_goes_away_with_the_folder() {
        let base =
            std::env::temp_dir().join(format!("pooprusteek-scratch-new-{}", std::process::id()));
        let scratch = Scratch::create(&base).unwrap();
        let first = scratch.write_system("one, longer").unwrap();
        let second = scratch.write_system("two").unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two");
        drop(scratch);
        assert!(!first.exists(), "the folder goes away after the call");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scratch_is_removed_with_its_owner() {
        let base = std::env::temp_dir().join("pooprusteek-scratch-test");
        let scratch = Scratch::create(&base).unwrap();
        let root = scratch.work_dir().parent().unwrap().to_path_buf();
        let system = scratch.write_system("hi").unwrap();
        assert!(system.is_file() && scratch.config_dir().is_dir());
        drop(scratch);
        assert!(!root.exists());
    }
}
