//! Бэкенд хранилища: куда физически кладутся значения по ключу.

use super::error::{SecretError, SecretResult};
use super::secret::Secret;

/// Что умеет любое место хранения. Синхронно: звать вне цикла событий.
pub trait SecretBackend: Send + Sync {
    fn name(&self) -> &'static str;
    /// `None` — записи нет; `Err` — хранилище не ответило или запись испорчена.
    fn get(&self, key: &str) -> SecretResult<Option<Secret>>;
    fn set(&self, key: &str, value: &Secret) -> SecretResult<()>;
    /// Удаляет запись. Повторное удаление не ошибка.
    fn delete(&self, key: &str) -> SecretResult<()>;
}

/// Хранилище, которое выключено намеренно (прогон с `--config`/`--data-dir`).
pub struct DisabledBackend {
    reason: String,
}

impl DisabledBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn err(&self, key: &str) -> SecretError {
        SecretError::Unavailable {
            name: key.to_string(),
            backend: self.name().to_string(),
            reason: self.reason.clone(),
        }
    }
}

impl SecretBackend for DisabledBackend {
    fn name(&self) -> &'static str {
        "disabled"
    }

    fn get(&self, key: &str) -> SecretResult<Option<Secret>> {
        Err(self.err(key))
    }

    fn set(&self, key: &str, _value: &Secret) -> SecretResult<()> {
        Err(self.err(key))
    }

    fn delete(&self, key: &str) -> SecretResult<()> {
        Err(self.err(key))
    }
}
