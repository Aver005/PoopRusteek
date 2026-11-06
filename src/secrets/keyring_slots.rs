//! Системное хранилище (Credential Manager, Keychain, Secret Service) через `keyring`.
//!
//! Вызовы синхронные и могут зависнуть на запросе разблокировки, поэтому каждый
//! идёт в отдельном потоке с таймаутом: чтение 5 с, запись 10 с.

use super::chunked::{ChunkedBackend, SlotError, Slots};
use std::sync::mpsc;
use std::time::Duration;

/// Служебное имя записей. MCP-токены лежат отдельно (`pooprusteek-mcp`).
pub const SERVICE: &str = "pooprusteek";
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct KeyringSlots {
    service: String,
}

impl KeyringSlots {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    pub fn backend(service: impl Into<String>) -> ChunkedBackend<Self> {
        ChunkedBackend::new(Self::new(service))
    }
}

/// Выполнить `job` в потоке и ждать не дольше `limit`. Зависший вызов бросаем:
/// поток доживёт сам, а цикл событий не ждёт.
fn with_timeout<T: Send + 'static>(
    limit: Duration,
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SlotError> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("keyring".to_string())
        .spawn(move || {
            let _ = tx.send(job());
        })
        .map_err(|e| SlotError::Unavailable(format!("cannot start a worker: {e}")))?;
    rx.recv_timeout(limit).map_err(|_| SlotError::Timeout)
}

/// Ошибку keyring разбираем по виду, без `Display` вариантов с байтами значения.
fn classify(error: keyring::Error) -> SlotError {
    use keyring::Error as E;
    match error {
        E::NoStorageAccess(platform) => SlotError::Unavailable(format!("no access: {platform}")),
        E::PlatformFailure(platform) => SlotError::Unavailable(format!("failure: {platform}")),
        E::TooLong(_, limit) => SlotError::TooLong(limit as usize),
        E::BadEncoding(_) | E::BadDataFormat(..) => {
            SlotError::Damaged("stored data is not valid text".to_string())
        }
        E::BadStoreFormat(_) => SlotError::Other("the store is damaged".to_string()),
        E::Invalid(attribute, _) => SlotError::Damaged(format!("invalid {attribute}")),
        E::Ambiguous(_) => SlotError::Damaged("several entries match".to_string()),
        _ => SlotError::Other("unsupported by this store".to_string()),
    }
}

impl Slots for KeyringSlots {
    fn name(&self) -> &'static str {
        "keyring"
    }

    fn read(&self, slot: &str) -> Result<Option<String>, SlotError> {
        let (service, slot) = (self.service.clone(), slot.to_string());
        with_timeout(READ_TIMEOUT, move || {
            let entry = keyring::Entry::new(&service, &slot).map_err(classify)?;
            match entry.get_password() {
                Ok(text) => Ok(Some(text)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(classify(error)),
            }
        })?
    }

    fn write(&self, slot: &str, text: &str) -> Result<(), SlotError> {
        let (service, slot, text) = (self.service.clone(), slot.to_string(), text.to_string());
        with_timeout(WRITE_TIMEOUT, move || {
            keyring::Entry::new(&service, &slot)
                .and_then(|entry| entry.set_password(&text))
                .map_err(classify)
        })?
    }

    fn remove(&self, slot: &str) -> Result<(), SlotError> {
        let (service, slot) = (self.service.clone(), slot.to_string());
        with_timeout(WRITE_TIMEOUT, move || {
            let entry = keyring::Entry::new(&service, &slot).map_err(classify)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(classify(error)),
            }
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::backend::SecretBackend;
    use crate::secrets::secret::Secret;

    /// Настоящее хранилище ОС: уникальное служебное имя, запись удаляется в конце.
    /// Длинное значение проверяет деление под предел Windows в 2560 байт.
    #[test]
    #[ignore = "нужно настоящее хранилище ОС и живая сессия пользователя"]
    fn real_keyring_round_trip_including_a_long_value() {
        let service = format!(
            "pooprusteek-selftest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let backend = KeyringSlots::backend(service);
        let short = Secret::new("selftest-short");
        let long = Secret::new("L".repeat(7_000));
        let outcome = (|| {
            backend.set("short", &short).map_err(|e| e.to_string())?;
            backend.set("long", &long).map_err(|e| e.to_string())?;
            let back_short = backend.get("short").map_err(|e| e.to_string())?;
            let back_long = backend.get("long").map_err(|e| e.to_string())?;
            Ok::<_, String>((back_short, back_long))
        })();
        // Чистим при любом исходе, до проверок.
        let _ = backend.delete("short");
        let _ = backend.delete("long");
        let (back_short, back_long) = outcome.expect("real keyring must accept the entries");
        assert_eq!(back_short.unwrap().expose(), short.expose());
        assert_eq!(back_long.unwrap().expose(), long.expose());
        assert!(
            backend.get("short").unwrap().is_none(),
            "entry must be gone"
        );
        assert!(backend.get("long").unwrap().is_none(), "entry must be gone");
    }
}
