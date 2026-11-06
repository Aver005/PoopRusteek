//! Длинные значения в хранилищах с малым пределом записи.
//!
//! Windows Credential Manager держит не больше 2560 байт на запись, а пароль
//! в нём лежит в UTF-16: безопасно только ~1280 символов. Значение режется на
//! куски по `CHUNK_BYTES`; первая запись несёт заголовок с числом частей и
//! контрольной суммой, остальные лежат в `<ключ>#<n>`.

use super::backend::SecretBackend;
use super::error::{SecretError, SecretResult};
use super::secret::Secret;
use sha2::{Digest, Sha256};

/// Байт UTF-8 в одном куске. Даже в UTF-16 это не больше 1800 байт.
pub const CHUNK_BYTES: usize = 900;
/// Потолок значения: 64 куска, около 57 КБ. Токенам этого хватает с запасом.
pub const MAX_PARTS: usize = 64;
const HEADER: &str = "psec1:";

/// Один слот хранилища: строка по имени. Меньше, чем `SecretBackend`.
pub trait Slots: Send + Sync {
    fn name(&self) -> &'static str;
    fn read(&self, slot: &str) -> Result<Option<String>, SlotError>;
    fn write(&self, slot: &str, text: &str) -> Result<(), SlotError>;
    fn remove(&self, slot: &str) -> Result<(), SlotError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotError {
    Unavailable(String),
    TooLong(usize),
    Timeout,
    Other(String),
}

/// `SecretBackend` поверх слотов: режет, склеивает, проверяет сумму.
pub struct ChunkedBackend<S: Slots> {
    slots: S,
}

impl<S: Slots> ChunkedBackend<S> {
    pub fn new(slots: S) -> Self {
        Self { slots }
    }

    fn map(&self, key: &str, error: SlotError) -> SecretError {
        let backend = self.slots.name().to_string();
        let name = key.to_string();
        match error {
            SlotError::Unavailable(reason) | SlotError::Other(reason) => SecretError::Unavailable {
                name,
                backend,
                reason,
            },
            SlotError::TooLong(limit) => SecretError::TooLarge {
                name,
                backend,
                limit,
            },
            SlotError::Timeout => SecretError::Timeout { name, backend },
        }
    }

    fn corrupt(&self, key: &str, reason: &str) -> SecretError {
        SecretError::Corrupt {
            name: key.to_string(),
            backend: self.slots.name().to_string(),
            reason: reason.to_string(),
        }
    }

    fn slot(key: &str, part: usize) -> String {
        if part == 0 {
            key.to_string()
        } else {
            format!("{key}#{part}")
        }
    }

    /// Число частей по заголовку первой записи; `1` для обычного значения.
    fn parts_of(first: &str) -> usize {
        parse_header(first).map_or(1, |(parts, _, _)| parts)
    }
}

fn checksum(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `(части, сумма, остаток первой записи)`.
fn parse_header(text: &str) -> Option<(usize, &str, &str)> {
    let rest = text.strip_prefix(HEADER)?;
    let (parts, rest) = rest.split_once(':')?;
    let (sum, body) = rest.split_once(':')?;
    let parts: usize = parts.parse().ok()?;
    (parts >= 1 && sum.len() == 16).then_some((parts, sum, body))
}

/// Режет по границе символа (инвариант 4): UTF-8 не рвётся посередине.
pub fn split_chunks(value: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let mut end = CHUNK_BYTES.min(rest.len());
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        chunks.push(head);
        rest = tail;
    }
    if chunks.is_empty() {
        chunks.push("");
    }
    chunks
}

impl<S: Slots> SecretBackend for ChunkedBackend<S> {
    fn name(&self) -> &'static str {
        self.slots.name()
    }

    fn get(&self, key: &str) -> SecretResult<Option<Secret>> {
        let first = match self.slots.read(key) {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(None),
            Err(error) => return Err(self.map(key, error)),
        };
        let Some((parts, sum, body)) = parse_header(&first) else {
            // Запись без заголовка: значение, положенное вне нас. Читаем как есть.
            if first.starts_with(HEADER) {
                return Err(self.corrupt(key, "unreadable header"));
            }
            return Ok(Some(Secret::new(first)));
        };
        let mut value = String::from(body);
        for part in 1..parts {
            match self.slots.read(&Self::slot(key, part)) {
                Ok(Some(text)) => value.push_str(&text),
                Ok(None) => return Err(self.corrupt(key, "a part is missing")),
                Err(error) => return Err(self.map(key, error)),
            }
        }
        if checksum(&value) != sum {
            return Err(self.corrupt(key, "checksum does not match"));
        }
        Ok(Some(Secret::new(value)))
    }

    fn set(&self, key: &str, value: &Secret) -> SecretResult<()> {
        let chunks = split_chunks(value.expose());
        if chunks.len() > MAX_PARTS {
            return Err(SecretError::TooLarge {
                name: key.to_string(),
                backend: self.slots.name().to_string(),
                limit: CHUNK_BYTES * MAX_PARTS,
            });
        }
        let previous = match self.slots.read(key) {
            Ok(Some(text)) => Self::parts_of(&text),
            _ => 1,
        };
        // Сначала хвост, потом заголовок: пока заголовок старый, старое значение читается целиком.
        for (index, chunk) in chunks.iter().enumerate().skip(1) {
            self.slots
                .write(&Self::slot(key, index), chunk)
                .map_err(|e| self.map(key, e))?;
        }
        let first = format!(
            "{HEADER}{}:{}:{}",
            chunks.len(),
            checksum(value.expose()),
            chunks[0]
        );
        self.slots
            .write(key, &first)
            .map_err(|e| self.map(key, e))?;
        for stale in chunks.len()..previous {
            let _ = self.slots.remove(&Self::slot(key, stale));
        }
        Ok(())
    }

    fn delete(&self, key: &str) -> SecretResult<()> {
        let parts = match self.slots.read(key) {
            Ok(Some(text)) => Self::parts_of(&text),
            Ok(None) => 1,
            Err(error) => return Err(self.map(key, error)),
        };
        self.slots.remove(key).map_err(|e| self.map(key, e))?;
        for part in 1..parts {
            let _ = self.slots.remove(&Self::slot(key, part));
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod fake {
    //! Слоты в памяти с настоящим пределом записи и поломками по требованию.
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    pub struct MemorySlots {
        pub limit: Option<usize>,
        pub store: Mutex<HashMap<String, String>>,
        pub fail_writes: AtomicBool,
    }

    impl MemorySlots {
        pub fn with_limit(limit: usize) -> Self {
            Self {
                limit: Some(limit),
                ..Self::default()
            }
        }
    }

    impl Slots for MemorySlots {
        fn name(&self) -> &'static str {
            "memory"
        }

        fn read(&self, slot: &str) -> Result<Option<String>, SlotError> {
            Ok(self.store.lock().unwrap().get(slot).cloned())
        }

        fn write(&self, slot: &str, text: &str) -> Result<(), SlotError> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(SlotError::Unavailable("write refused".into()));
            }
            if let Some(limit) = self.limit
                && text.encode_utf16().count() * 2 > limit
            {
                return Err(SlotError::TooLong(limit));
            }
            self.store
                .lock()
                .unwrap()
                .insert(slot.to_string(), text.to_string());
            Ok(())
        }

        fn remove(&self, slot: &str) -> Result<(), SlotError> {
            self.store.lock().unwrap().remove(slot);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::MemorySlots;
    use super::*;
    use std::sync::atomic::Ordering;

    fn windows_like() -> ChunkedBackend<MemorySlots> {
        ChunkedBackend::new(MemorySlots::with_limit(2560))
    }

    fn entries(backend: &ChunkedBackend<MemorySlots>) -> usize {
        backend.slots.store.lock().unwrap().len()
    }

    #[test]
    fn a_short_value_round_trips_in_one_entry() {
        let backend = windows_like();
        backend.set("k", &Secret::new("sk-short")).unwrap();
        assert_eq!(backend.get("k").unwrap().unwrap().expose(), "sk-short");
        assert_eq!(entries(&backend), 1);
    }

    #[test]
    fn a_value_over_the_windows_limit_is_split_and_restored() {
        let backend = windows_like();
        let long = "x".repeat(6_000);
        backend.set("k", &Secret::new(long.clone())).unwrap();
        assert!(entries(&backend) > 2);
        assert_eq!(backend.get("k").unwrap().unwrap().expose(), long);
    }

    #[test]
    fn multibyte_text_is_never_cut_inside_a_character() {
        let backend = windows_like();
        let long = "ключ-🔑-".repeat(700);
        backend.set("k", &Secret::new(long.clone())).unwrap();
        assert_eq!(backend.get("k").unwrap().unwrap().expose(), long);
    }

    #[test]
    fn shrinking_a_value_drops_the_stale_parts() {
        let backend = windows_like();
        backend.set("k", &Secret::new("y".repeat(5_000))).unwrap();
        backend.set("k", &Secret::new("short")).unwrap();
        assert_eq!(entries(&backend), 1);
        assert_eq!(backend.get("k").unwrap().unwrap().expose(), "short");
    }

    #[test]
    fn a_missing_part_is_reported_as_damage_not_a_wrong_value() {
        let backend = windows_like();
        backend.set("k", &Secret::new("z".repeat(3_000))).unwrap();
        backend.slots.store.lock().unwrap().remove("k#1");
        assert!(matches!(backend.get("k"), Err(SecretError::Corrupt { .. })));
    }

    #[test]
    fn a_swapped_part_fails_the_checksum() {
        let backend = windows_like();
        backend.set("k", &Secret::new("a".repeat(3_000))).unwrap();
        backend
            .slots
            .store
            .lock()
            .unwrap()
            .insert("k#1".into(), "b".repeat(900));
        assert!(matches!(backend.get("k"), Err(SecretError::Corrupt { .. })));
    }

    #[test]
    fn a_value_beyond_the_part_cap_is_refused_with_a_clear_error() {
        let backend = windows_like();
        let error = backend
            .set("k", &Secret::new("q".repeat(CHUNK_BYTES * MAX_PARTS + 1)))
            .unwrap_err();
        assert!(matches!(error, SecretError::TooLarge { .. }), "{error}");
    }

    #[test]
    fn an_old_entry_without_a_header_is_read_as_is() {
        let backend = windows_like();
        backend
            .slots
            .store
            .lock()
            .unwrap()
            .insert("k".into(), "legacy-value".into());
        assert_eq!(backend.get("k").unwrap().unwrap().expose(), "legacy-value");
    }

    #[test]
    fn delete_removes_every_part_and_is_repeatable() {
        let backend = windows_like();
        backend.set("k", &Secret::new("w".repeat(4_000))).unwrap();
        backend.delete("k").unwrap();
        backend.delete("k").unwrap();
        assert_eq!(entries(&backend), 0);
        assert!(backend.get("k").unwrap().is_none());
    }

    #[test]
    fn a_failed_write_leaves_the_previous_value_readable() {
        let backend = windows_like();
        backend.set("k", &Secret::new("old")).unwrap();
        backend.slots.fail_writes.store(true, Ordering::SeqCst);
        assert!(backend.set("k", &Secret::new("n".repeat(3_000))).is_err());
        backend.slots.fail_writes.store(false, Ordering::SeqCst);
        assert_eq!(backend.get("k").unwrap().unwrap().expose(), "old");
    }

    #[test]
    fn error_text_never_contains_the_value() {
        let backend = ChunkedBackend::new(MemorySlots::with_limit(10));
        let error = backend
            .set("name", &Secret::new("sk-canary-VALUE"))
            .unwrap_err();
        assert!(!error.to_string().contains("canary"), "{error}");
    }
}
