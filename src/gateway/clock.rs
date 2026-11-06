//! Часы шлюза. Всё, что зависит от «сейчас» (cooldown, возраст снимков), берёт время
//! отсюда, поэтому тесты двигают часы рукой и не спят.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Секунды Unix для файлов на диске; время до эпохи даёт 0.
pub fn to_unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn from_unix(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Часы, которые идут только по команде теста.
#[cfg(test)]
pub struct ManualClock {
    now: std::sync::Mutex<SystemTime>,
}

#[cfg(test)]
impl ManualClock {
    pub fn new(start_secs: u64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            now: std::sync::Mutex::new(from_unix(start_secs)),
        })
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().expect("clock lock");
        *now += by;
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().expect("clock lock")
    }
}

/// Временная папка для теста; чистить не нужно, система сама.
#[cfg(test)]
pub fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pooprusteek-gw-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(1_000);
        let before = clock.now();
        assert_eq!(clock.now(), before, "time stands still");
        clock.advance(Duration::from_secs(30));
        assert_eq!(to_unix(clock.now()), 1_030);
    }

    #[test]
    fn unix_seconds_round_trip() {
        assert_eq!(to_unix(from_unix(86_400)), 86_400);
        assert_eq!(
            to_unix(UNIX_EPOCH - Duration::from_secs(5)),
            0,
            "pre-epoch is zero"
        );
    }
}
