//! `SecretStore`: бэкенд + кэш на время процесса + проверка «записал — прочитал — сравнил».

use super::backend::{DisabledBackend, SecretBackend};
use super::error::{SecretError, SecretResult};
use super::keyring_slots::{KeyringSlots, SERVICE};
use super::secret::Secret;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Как долго помним «хранилище не отвечает», чтобы не ждать таймаут на каждом чтении.
const DOWN_MEMORY: Duration = Duration::from_secs(30);

pub struct SecretStore {
    backend: Arc<dyn SecretBackend>,
    cache: Mutex<HashMap<String, Secret>>,
    down: Mutex<Option<(Instant, SecretError)>>,
}

impl SecretStore {
    pub fn new(backend: Arc<dyn SecretBackend>) -> Self {
        Self {
            backend,
            cache: Mutex::new(HashMap::new()),
            down: Mutex::new(None),
        }
    }

    /// Системное хранилище пользователя.
    pub fn system() -> Self {
        Self::new(Arc::new(KeyringSlots::backend(SERVICE)))
    }

    /// Хранилище выключено: изолированный прогон не должен трогать настоящие ключи.
    pub fn disabled(reason: &str) -> Self {
        Self::new(Arc::new(DisabledBackend::new(reason)))
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Последняя известная причина недоступности, если она ещё свежа.
    pub fn known_down(&self) -> Option<SecretError> {
        let mut down = self.down.lock().ok()?;
        match down.as_ref() {
            Some((at, error)) if at.elapsed() < DOWN_MEMORY => Some(error.clone()),
            Some(_) => {
                *down = None;
                None
            }
            None => None,
        }
    }

    pub fn forget_down(&self) {
        if let Ok(mut down) = self.down.lock() {
            *down = None;
        }
    }

    fn note(&self, error: &SecretError) {
        if error.is_unavailable()
            && let Ok(mut down) = self.down.lock()
        {
            *down = Some((Instant::now(), error.clone()));
        }
    }

    pub fn get(&self, id: &str) -> SecretResult<Option<Secret>> {
        if let Some(value) = self.cache.lock().ok().and_then(|c| c.get(id).cloned()) {
            return Ok(Some(value));
        }
        if let Some(error) = self.known_down() {
            return Err(error);
        }
        match self.backend.get(id) {
            Ok(Some(value)) => {
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(id.to_string(), value.clone());
                }
                Ok(Some(value))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.note(&error);
                Err(error)
            }
        }
    }

    /// Записывает, читает обратно мимо кэша и сравнивает. Ошибка — запись не считается.
    pub fn set_verified(&self, id: &str, value: &Secret) -> SecretResult<()> {
        if let Err(error) = self.backend.set(id, value) {
            self.note(&error);
            return Err(error);
        }
        let back = match self.backend.get(id) {
            Ok(back) => back,
            Err(error) => {
                self.note(&error);
                return Err(error);
            }
        };
        match back {
            Some(back) if back.matches(value) => {
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(id.to_string(), value.clone());
                }
                Ok(())
            }
            _ => Err(SecretError::Mismatch {
                name: id.to_string(),
                backend: self.backend.name().to_string(),
            }),
        }
    }

    pub fn delete(&self, id: &str) -> SecretResult<()> {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(id);
        }
        self.backend.delete(id).inspect_err(|e| self.note(e))
    }

    /// Запись, чтение и удаление служебной записи: хранилище живо или нет.
    pub fn probe(&self) -> SecretResult<()> {
        let id = format!(
            "probe/{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let outcome = self.set_verified(&id, &Secret::new("probe"));
        let _ = self.delete(&id);
        outcome
    }
}

static PROCESS_STORE: OnceLock<Arc<SecretStore>> = OnceLock::new();

/// Хранилище процесса. В изолированном прогоне (`--config`/`--data-dir`) оно выключено.
pub fn process_store() -> Arc<SecretStore> {
    Arc::clone(PROCESS_STORE.get_or_init(|| {
        // Юнит-тесты настоящее хранилище разработчика не видят никогда.
        Arc::new(store_for(cfg!(test) || crate::config::is_isolated()))
    }))
}

/// Изолированному прогону настоящее хранилище не отдаётся ни на чтение, ни на запись.
fn store_for(isolated: bool) -> SecretStore {
    if isolated {
        SecretStore::disabled("disabled for runs with --config or --data-dir")
    } else {
        SecretStore::system()
    }
}

#[cfg(test)]
pub mod fake {
    //! Хранилище в памяти с поломками по требованию для тестов миграции.
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        Healthy = 0,
        /// Запись не проходит.
        FailWrite = 1,
        /// Запись проходит, чтение после неё падает.
        FailRead = 2,
        /// Запись проходит, читается другое значение.
        Differs = 3,
        /// Хранилища нет вовсе.
        Unavailable = 4,
    }

    #[derive(Default)]
    pub struct FakeBackend {
        pub data: Mutex<HashMap<String, Secret>>,
        mode: AtomicU8,
    }

    impl FakeBackend {
        pub fn mode(&self, mode: Mode) {
            self.mode.store(mode as u8, Ordering::SeqCst);
        }

        fn is(&self, mode: Mode) -> bool {
            self.mode.load(Ordering::SeqCst) == mode as u8
        }

        /// Поломки точечные касаются настоящих секретов, а не служебной пробы:
        /// так видно, что делает миграция, когда хранилище «живо», но врёт на ключе.
        fn faulty(&self, mode: Mode, key: &str) -> bool {
            self.is(mode) && !key.starts_with("probe/")
        }

        fn down(&self, key: &str) -> SecretError {
            SecretError::Unavailable {
                name: key.to_string(),
                backend: "fake".into(),
                reason: "no store".into(),
            }
        }
    }

    impl SecretBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn get(&self, key: &str) -> SecretResult<Option<Secret>> {
            if self.is(Mode::Unavailable) || self.faulty(Mode::FailRead, key) {
                return Err(self.down(key));
            }
            let found = self.data.lock().unwrap().get(key).cloned();
            if self.faulty(Mode::Differs, key) && found.is_some() {
                return Ok(Some(Secret::new("something-else")));
            }
            Ok(found)
        }

        fn set(&self, key: &str, value: &Secret) -> SecretResult<()> {
            if self.is(Mode::Unavailable) || self.faulty(Mode::FailWrite, key) {
                return Err(self.down(key));
            }
            self.data
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        fn delete(&self, key: &str) -> SecretResult<()> {
            if self.is(Mode::Unavailable) {
                return Err(self.down(key));
            }
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
    }

    pub fn store() -> (Arc<FakeBackend>, SecretStore) {
        let backend = Arc::new(FakeBackend::default());
        let store = SecretStore::new(backend.clone());
        (backend, store)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Mode, store};
    use super::*;

    #[test]
    fn set_verified_stores_and_reads_back() {
        let (_, store) = store();
        store.set_verified("a", &Secret::new("v")).unwrap();
        assert_eq!(store.get("a").unwrap().unwrap().expose(), "v");
    }

    #[test]
    fn a_write_that_reads_back_differently_is_a_mismatch() {
        let (backend, store) = store();
        backend.mode(Mode::Differs);
        let error = store.set_verified("a", &Secret::new("v")).unwrap_err();
        assert!(matches!(error, SecretError::Mismatch { .. }), "{error}");
        assert!(
            store.cache.lock().unwrap().is_empty(),
            "no caching of a bad write"
        );
    }

    #[test]
    fn an_unavailable_backend_is_remembered_so_reads_do_not_wait_again() {
        let (backend, store) = store();
        backend.mode(Mode::Unavailable);
        assert!(store.get("a").unwrap_err().is_unavailable());
        backend.mode(Mode::Healthy);
        // Помним отказ: повторное чтение не ходит в хранилище.
        assert!(store.get("a").unwrap_err().is_unavailable());
        store.forget_down();
        assert!(store.get("a").unwrap().is_none());
    }

    #[test]
    fn probe_leaves_nothing_behind() {
        let (backend, store) = store();
        store.probe().unwrap();
        assert!(backend.data.lock().unwrap().is_empty());
        backend.mode(Mode::Unavailable);
        assert!(store.probe().is_err());
    }

    #[test]
    fn an_isolated_run_gets_a_store_that_never_reaches_the_real_keyring() {
        let store = store_for(true);
        assert_eq!(store.backend_name(), "disabled");
        assert!(store.get("global/deepseek-token").is_err());
        assert!(store.set_verified("global/x", &Secret::new("v")).is_err());
        assert!(store.delete("global/x").is_err());
        assert!(store.probe().is_err());
        assert_eq!(store_for(false).backend_name(), "keyring");
    }

    #[test]
    fn a_disabled_store_refuses_everything_with_its_reason() {
        let store = SecretStore::disabled("harness run");
        let error = store.get("x").unwrap_err();
        assert!(error.is_unavailable());
        assert!(error.to_string().contains("harness run"));
        assert!(store.set_verified("x", &Secret::new("v")).is_err());
    }

    /// Одна повреждённая запись — это «не прочитали её», а не «хранилище недоступно»: остальные
    /// секреты читаются, и по ним не запоминается отказ на 30 секунд.
    #[test]
    fn one_damaged_entry_does_not_make_the_whole_store_unavailable() {
        use crate::secrets::chunked::{ChunkedBackend, SlotError, Slots};
        struct OneBad(std::sync::Mutex<std::collections::HashMap<String, String>>);
        impl Slots for OneBad {
            fn name(&self) -> &'static str {
                "fake"
            }
            fn read(&self, slot: &str) -> Result<Option<String>, SlotError> {
                if slot == "bad" {
                    return Err(SlotError::Damaged("not valid text".into()));
                }
                Ok(self.0.lock().unwrap().get(slot).cloned())
            }
            fn write(&self, slot: &str, text: &str) -> Result<(), SlotError> {
                self.0.lock().unwrap().insert(slot.into(), text.into());
                Ok(())
            }
            fn remove(&self, slot: &str) -> Result<(), SlotError> {
                self.0.lock().unwrap().remove(slot);
                Ok(())
            }
        }
        let store = SecretStore::new(Arc::new(ChunkedBackend::new(OneBad(Default::default()))));
        store
            .set_verified("good", &Secret::new("fine-value"))
            .unwrap();
        let error = store.get("bad").unwrap_err();
        assert!(matches!(error, SecretError::Corrupt { .. }), "{error}");
        assert!(!error.is_unavailable());
        assert!(store.known_down().is_none(), "no 30-second blackout");
        // Рядом лежащий секрет читается, но кэш уже есть — проверяем и мимо кэша.
        let fresh = SecretStore::new(Arc::new(ChunkedBackend::new(OneBad(Default::default()))));
        fresh
            .set_verified("other", &Secret::new("other-value"))
            .unwrap();
        let _ = fresh.get("bad");
        assert_eq!(fresh.get("other").unwrap().unwrap().expose(), "other-value");
    }
}
