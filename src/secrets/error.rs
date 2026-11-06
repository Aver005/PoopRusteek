//! Ошибки секретов. В тексте только имя секрета и причина, никогда значение.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SecretError {
    #[error("secret '{name}' is not set in {backend}")]
    NotFound { name: String, backend: String },
    #[error("{backend} is unavailable for '{name}': {reason}")]
    Unavailable {
        name: String,
        backend: String,
        reason: String,
    },
    #[error("secret '{name}' is too large for {backend} ({limit} bytes per entry)")]
    TooLarge {
        name: String,
        backend: String,
        limit: usize,
    },
    #[error("secret '{name}' in {backend} is damaged: {reason}")]
    Corrupt {
        name: String,
        backend: String,
        reason: String,
    },
    #[error("secret '{name}' was written to {backend} but reading it back gave a different value")]
    Mismatch { name: String, backend: String },
    #[error("{backend} did not answer for '{name}' in time")]
    Timeout { name: String, backend: String },
    #[error("secret reference '{name}' is not allowed here: {reason}")]
    Refused { name: String, reason: String },
    #[error(
        "secret '{name}' is not trusted for host '{host}'; confirm it with `pooprusteek secrets bind {name} {host}`"
    )]
    HostNotTrusted { name: String, host: String },
    #[error("account '{name}' needs no secret")]
    NotNeeded { name: String },
}

impl SecretError {
    /// Недоступность хранилища — не «секрета нет»: значение может лежать на месте.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Timeout { .. })
    }
}

pub type SecretResult<T> = Result<T, SecretError>;
