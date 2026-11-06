//! Межпроцессная блокировка на чтение-правку-запись файла.
//!
//! `atomic_write` атомарен на файл, но два процесса, которые читают, правят и
//! пишут по очереди, затирают правки друг друга. Блокировка — файл `<цель>.lock`,
//! созданный через `create_new`; брошенный (старше 30 с) снимается.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Ожидание больше срока «брошенного» замка: замок упавшего процесса можно и переждать, и забрать.
/// Срок щедрый: держатель замка ходит в хранилище (до 10 с на запись) и пишет на медленный диск.
const STALE: Duration = Duration::from_secs(60);
const WAIT: Duration = Duration::from_secs(75);
const POLL: Duration = Duration::from_millis(25);

pub struct FileLock {
    path: PathBuf,
}

/// На Windows файл, который только что заменили, на миг бывает занят антивирусом или
/// индексатором: чтение и запись отвечают «отказано в доступе». Повторяем несколько раз.
pub fn retry_transient<T>(mut action: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempt = 0u32;
    loop {
        match action() {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied && attempt < 5 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(30 * u64::from(attempt)));
            }
            other => return other,
        }
    }
}

pub fn lock_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    target.with_file_name(name)
}

impl FileLock {
    pub fn acquire(target: &Path) -> std::io::Result<Self> {
        Self::acquire_within(target, WAIT, STALE)
    }

    pub fn acquire_within(target: &Path, wait: Duration, stale: Duration) -> std::io::Result<Self> {
        let path = lock_path(target);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                // На Windows файл блокировки, который прямо сейчас удаляют, отвечает «отказано
                // в доступе», а не «уже есть»: ждём так же.
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    // Держатель умер (процесс-то нет): ждать минуту «брошенности» незачем.
                    if holder_is_dead(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    let age = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| SystemTime::now().duration_since(m).ok());
                    if age.is_some_and(|age| age > stale) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() > wait {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("{} is locked by another process", target.display()),
                        ));
                    }
                    std::thread::sleep(POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Процесс с таким pid точно не существует. Сомнение (нет прав, pid не прочитан) — «жив».
fn pid_is_dead(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: сигнал 0 ничего не доставляет, только проверяет существование.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: обычные вызовы Win32; хэндл закрывается ниже.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return GetLastError() == ERROR_INVALID_PARAMETER;
            }
            let mut code = 0u32;
            let known = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            known && code != STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Файл замка называет умерший процесс. Свой pid и пустой файл (ещё не записан) — не мёртвые.
fn holder_is_dead(lock: &Path) -> bool {
    std::fs::read_to_string(lock)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .is_some_and(|pid| pid != std::process::id() && pid_is_dead(pid))
}

impl FileLock {
    /// Пульс долгой работы под замком: свежее время файла не даёт другому счесть замок брошенным.
    pub fn touch(&self) {
        if let Ok(file) = OpenOptions::new().write(true).open(&self.path) {
            let _ = file.set_modified(SystemTime::now());
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-lock-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_second_holder_waits_for_the_first() {
        let dir = dir("wait");
        let target = dir.join("config.toml");
        let first = FileLock::acquire(&target).unwrap();
        let blocked = FileLock::acquire_within(&target, Duration::from_millis(120), STALE);
        assert!(blocked.is_err());
        drop(first);
        assert!(FileLock::acquire(&target).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_abandoned_lock_is_taken_over() {
        let dir = dir("stale");
        let target = dir.join("config.toml");
        std::fs::write(lock_path(&target), "999999").unwrap();
        let taken = FileLock::acquire_within(&target, Duration::from_secs(2), Duration::ZERO);
        assert!(taken.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pid мёртвого процесса: запускаем короткий и ждём его конца.
    fn dead_pid() -> u32 {
        let mut command = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit"]);
            c
        } else {
            std::process::Command::new("true")
        };
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// Упавший держатель не заставляет ждать минуту: его pid мёртв, замок забирается сразу.
    #[test]
    fn a_lock_of_a_dead_process_is_taken_at_once_and_a_live_one_is_not() {
        let dir = dir("deadpid");
        let target = dir.join("config.toml");
        std::fs::write(lock_path(&target), dead_pid().to_string()).unwrap();
        let started = Instant::now();
        // «Брошенным» по возрасту замок не считается (час), ждать разрешено 20 с.
        let taken =
            FileLock::acquire_within(&target, Duration::from_secs(20), Duration::from_secs(3600));
        assert!(taken.is_ok());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        drop(taken);

        // Живой держатель (мы сами с другим «потоком»: pid свой) замок не теряет.
        let first = FileLock::acquire(&target).unwrap();
        let blocked = FileLock::acquire_within(
            &target,
            Duration::from_millis(150),
            Duration::from_secs(3600),
        );
        assert!(blocked.is_err());
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn many_threads_serialise_their_counter_updates() {
        let dir = dir("race");
        let target = dir.join("counter.txt");
        std::fs::write(&target, "0").unwrap();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let target = target.clone();
                std::thread::spawn(move || {
                    for _ in 0..3 {
                        // fsync на медленном диске: ждём с запасом, проверяем порядок, а не скорость.
                        let _guard =
                            FileLock::acquire_within(&target, Duration::from_secs(120), STALE)
                                .unwrap();
                        let current: u32 = retry_transient(|| std::fs::read_to_string(&target))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        retry_transient(|| {
                            crate::util::atomic_write(&target, (current + 1).to_string().as_bytes())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap().trim(), "12");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Замок, оставленный упавшим процессом секунду назад, переживается: ожидание дольше срока
    /// «брошенного», иначе его нельзя было бы ни взять, ни переждать.
    #[test]
    fn waiting_outlasts_the_time_after_which_a_lock_counts_as_abandoned() {
        assert!(WAIT > STALE);
    }
}
