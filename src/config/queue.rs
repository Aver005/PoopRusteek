//! Очередь записи конфига вне цикла событий.
//!
//! Сохранение берёт файловый замок (до 15 с) и пишет на диск; на цикле событий этого делать
//! нельзя. Задания идут одним рабочим потоком строго по порядку, результат-ошибка приходит
//! уведомлением, перед выходом очередь сбрасывается (`flush`).

use super::load::{SaveJob, execute_job};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

type Notifier = Arc<dyn Fn(String) + Send + Sync>;

enum Message {
    Save(SaveJob),
    Flush(mpsc::Sender<()>),
}

pub struct SaveQueue {
    tx: Mutex<mpsc::Sender<Message>>,
    pending: Arc<AtomicUsize>,
}

impl SaveQueue {
    /// Запускает рабочий поток. `notify` получает текст ошибки, когда сохранение не удалось.
    pub fn start(notify: Notifier) -> Self {
        let (tx, rx) = mpsc::channel::<Message>();
        let pending = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&pending);
        let _ = std::thread::Builder::new()
            .name("config-writer".to_string())
            .spawn(move || {
                while let Ok(message) = rx.recv() {
                    match message {
                        Message::Save(job) => {
                            if let Err(error) = execute_job(job) {
                                notify(format!("Failed to save config: {error}"));
                            }
                            counter.fetch_sub(1, Ordering::SeqCst);
                        }
                        Message::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            });
        Self {
            tx: Mutex::new(tx),
            pending,
        }
    }

    /// Ставит задание в очередь и сразу возвращается.
    pub(super) fn submit(&self, job: SaveJob) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        let sent = self
            .tx
            .lock()
            .map(|tx| tx.send(Message::Save(job)).is_ok())
            .unwrap_or(false);
        if !sent {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Ждёт, пока всё поставленное до этого момента записано. `false` — не дождались.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (ack_tx, ack_rx) = mpsc::channel();
        let sent = self
            .tx
            .lock()
            .map(|tx| tx.send(Message::Flush(ack_tx)).is_ok())
            .unwrap_or(false);
        sent && ack_rx.recv_timeout(timeout).is_ok()
    }
}

static GLOBAL: OnceLock<SaveQueue> = OnceLock::new();

/// Включает фоновое сохранение для процесса. Без этого `config::save` пишет на месте
/// (CLI, тесты, харнесс). Зовёт TUI один раз при старте.
pub fn enable_background_saves(notify: impl Fn(String) + Send + Sync + 'static) {
    let _ = GLOBAL.set(SaveQueue::start(Arc::new(notify)));
}

pub(super) fn global() -> Option<&'static SaveQueue> {
    GLOBAL.get()
}

/// Дожидается записи всего, что поставлено в очередь; без очереди ничего не делает.
pub fn flush_saves(timeout: Duration) -> bool {
    global().is_none_or(|queue| queue.flush(timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::lock::FileLock;
    use crate::config::{Config, load};
    use std::path::PathBuf;
    use std::time::Instant;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-queue-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_for(path: &PathBuf, theme: &str) -> Config {
        let mut config = Config::default();
        config.source.write_path = Some(path.clone());
        config.ui.theme = theme.to_string();
        config
    }

    fn silent() -> Notifier {
        Arc::new(|_| {})
    }

    /// Чужой замок держит файл, а вызывающий не ждёт ни секунды.
    #[test]
    fn a_held_lock_never_stalls_the_caller_and_the_save_lands_afterwards() {
        let root = dir("lock");
        let path = root.join("config.toml");
        let queue = SaveQueue::start(silent());
        let guard = FileLock::acquire(&path).unwrap();

        let started = Instant::now();
        queue.submit(load::prepare_job(&config_for(&path, "nord")).unwrap());
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "submit must return at once, took {:?}",
            started.elapsed()
        );
        assert_eq!(queue.pending(), 1);
        assert!(!path.exists(), "nothing is written while the lock is held");

        drop(guard);
        assert!(queue.flush(Duration::from_secs(20)));
        assert!(std::fs::read_to_string(&path).unwrap().contains("nord"));
        assert_eq!(queue.pending(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Две быстрые правки подряд: последняя остаётся в файле, а не первая.
    #[test]
    fn two_quick_saves_keep_their_order() {
        let root = dir("order");
        let path = root.join("config.toml");
        // Тест падал один раз за пять полных прогонов: ошибки записи должны быть видны.
        let (tx, rx) = mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let queue = SaveQueue::start(Arc::new(move |message| {
            let _ = tx.lock().map(|tx| tx.send(message));
        }));
        let first = config_for(&path, "first-theme");
        let mut second = first.clone();
        second.ui.theme = "second-theme".to_string();
        for _ in 0..5 {
            queue.submit(load::prepare_job(&first).unwrap());
            queue.submit(load::prepare_job(&second).unwrap());
        }
        let flushed = queue.flush(Duration::from_secs(20));
        let errors: Vec<String> = rx.try_iter().collect();
        assert!(flushed, "flush timed out, save errors: {errors:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("second-theme") && !text.contains("first-theme"),
            "save errors: {errors:?}\n{text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ошибка записи приходит уведомлением; повреждённый файл остаётся как был.
    #[test]
    fn a_failed_save_is_reported_through_the_notifier() {
        let root = dir("notify");
        let path = root.join("config.toml");
        std::fs::write(&path, "[ui\n").unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let queue = SaveQueue::start(Arc::new(move |message| {
            let _ = tx.lock().unwrap().send(message);
        }));
        queue.submit(load::prepare_job(&config_for(&path, "nord")).unwrap());
        assert!(queue.flush(Duration::from_secs(20)));
        let message = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(message.contains("Failed to save config"), "{message}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[ui\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flushing_an_idle_queue_returns_at_once() {
        let queue = SaveQueue::start(silent());
        let started = Instant::now();
        assert!(queue.flush(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
