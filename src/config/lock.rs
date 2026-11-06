//! Межпроцессная блокировка на чтение-правку-запись файла.
//!
//! `atomic_write` атомарен на файл, но два процесса, которые читают, правят и
//! пишут по очереди, затирают правки друг друга. Блокировка — файл `<цель>.lock`,
//! созданный через `create_new`; брошенный (старше 30 с) снимается.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const WAIT: Duration = Duration::from_secs(15);
const STALE: Duration = Duration::from_secs(30);
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
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
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
}
