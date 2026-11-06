//! Длинные значения в хранилищах с малым пределом записи.
//!
//! Windows Credential Manager держит не больше 2560 байт на запись, а пароль
//! в нём лежит в UTF-16: безопасно только ~1280 символов. Значение режется на
//! куски по `CHUNK_BYTES`; первая запись несёт заголовок с поколением, числом
//! частей и контрольной суммой, остальные лежат в `<ключ>#g<поколение>#<n>`.
//!
//! Перезапись идёт в НОВОЕ поколение кусков, заголовок переключается последним, и только
//! потом старое поколение удаляется. Падение на любом шаге оставляет читаемым либо старое
//! значение целиком, либо новое целиком, но не смесь.

use super::backend::SecretBackend;
use super::error::{SecretError, SecretResult};
use super::secret::Secret;
use sha2::{Digest, Sha256};

/// Байт UTF-8 в одном куске. Даже в UTF-16 это не больше 1800 байт.
pub const CHUNK_BYTES: usize = 900;
/// Потолок значения: 64 куска, около 57 КБ. Токенам этого хватает с запасом.
pub const MAX_PARTS: usize = 64;
/// Первая версия: куски `<ключ>#<n>`, без поколения. Читается, пишется уже вторая.
const HEADER_V1: &str = "psec1:";
const HEADER_V2: &str = "psec2:";

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
    /// Запись на месте, но её не разобрать (не текст, неоднозначна): повреждена одна запись,
    /// а не хранилище целиком.
    Damaged(String),
    TooLong(usize),
    Timeout,
    Other(String),
}

/// `SecretBackend` поверх слотов: режет, склеивает, проверяет сумму.
pub struct ChunkedBackend<S: Slots> {
    slots: S,
}

/// Заголовок первой записи. `generation == 0` — формат первой версии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header<'a> {
    generation: u32,
    parts: usize,
    sum: &'a str,
    body: &'a str,
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
            SlotError::Damaged(reason) => SecretError::Corrupt {
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

    /// Имя слота куска `part >= 1` данного поколения.
    fn slot(key: &str, generation: u32, part: usize) -> String {
        if generation == 0 {
            format!("{key}#{part}")
        } else {
            format!("{key}#g{generation}#{part}")
        }
    }

    /// Убирает куски `1..parts` поколения. Первая ошибка прерывает: хранилище не отвечает.
    fn remove_parts(&self, key: &str, generation: u32, parts: usize) -> Result<(), SlotError> {
        for part in 1..parts {
            self.slots.remove(&Self::slot(key, generation, part))?;
        }
        Ok(())
    }
}

fn checksum(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn parse_header(text: &str) -> Option<Header<'_>> {
    if let Some(rest) = text.strip_prefix(HEADER_V2) {
        let (generation, rest) = rest.split_once(':')?;
        let (parts, rest) = rest.split_once(':')?;
        let (sum, body) = rest.split_once(':')?;
        let (generation, parts): (u32, usize) = (generation.parse().ok()?, parts.parse().ok()?);
        return (generation >= 1 && (1..=MAX_PARTS).contains(&parts) && sum.len() == 16).then_some(
            Header {
                generation,
                parts,
                sum,
                body,
            },
        );
    }
    let rest = text.strip_prefix(HEADER_V1)?;
    let (parts, rest) = rest.split_once(':')?;
    let (sum, body) = rest.split_once(':')?;
    let parts: usize = parts.parse().ok()?;
    ((1..=MAX_PARTS).contains(&parts) && sum.len() == 16).then_some(Header {
        generation: 0,
        parts,
        sum,
        body,
    })
}

fn looks_like_header(text: &str) -> bool {
    text.starts_with(HEADER_V1) || text.starts_with(HEADER_V2)
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
        let Some(header) = parse_header(&first) else {
            // Запись без заголовка: значение, положенное вне нас. Читаем как есть.
            if looks_like_header(&first) {
                return Err(self.corrupt(key, "unreadable header"));
            }
            return Ok(Some(Secret::new(first)));
        };
        let mut value = String::from(header.body);
        for part in 1..header.parts {
            match self.slots.read(&Self::slot(key, header.generation, part)) {
                Ok(Some(text)) => value.push_str(&text),
                Ok(None) => return Err(self.corrupt(key, "a part is missing")),
                Err(error) => return Err(self.map(key, error)),
            }
        }
        if checksum(&value) != header.sum {
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
        // Что лежит сейчас: поколение и число частей, чтобы выбрать новое имя и убрать старое.
        let (prev_generation, prev_parts) = match self.slots.read(key) {
            Ok(Some(text)) => parse_header(&text).map_or((0, 1), |h| (h.generation, h.parts)),
            _ => (0, 1),
        };
        let generation = prev_generation.wrapping_add(1).max(1);
        // 1) Новое поколение кусков под новыми именами: старое значение не задето.
        for (index, chunk) in chunks.iter().enumerate().skip(1) {
            self.slots
                .write(&Self::slot(key, generation, index), chunk)
                .map_err(|e| self.map(key, e))?;
        }
        // 2) Одна запись переключает значение целиком.
        let first = format!(
            "{HEADER_V2}{generation}:{}:{}:{}",
            chunks.len(),
            checksum(value.expose()),
            chunks[0]
        );
        self.slots
            .write(key, &first)
            .map_err(|e| self.map(key, e))?;
        // 3) Старое поколение больше не читается; убираем по возможности.
        let _ = self.remove_parts(key, prev_generation, prev_parts);
        Ok(())
    }

    fn delete(&self, key: &str) -> SecretResult<()> {
        let header_generation = match self.slots.read(key) {
            Ok(Some(text)) => parse_header(&text).map(|h| h.generation),
            Ok(None) => None,
            Err(error) => return Err(self.map(key, error)),
        };
        self.slots.remove(key).map_err(|e| self.map(key, e))?;
        // Куски прошлого, текущего и следующего поколений: недописанная перезапись оставляет
        // хвосты под именем следующего, недочищенная — под именем прошлого. Заголовок мог
        // не читаться — тогда подметаем первые поколения.
        let generations: Vec<u32> = match header_generation {
            Some(g) => vec![g.saturating_sub(1), g, g + 1],
            None => vec![0, 1, 2],
        };
        let mut stopped = None;
        'sweep: for generation in generations {
            for part in 1..MAX_PARTS {
                if let Err(error) = self.slots.remove(&Self::slot(key, generation, part)) {
                    // Хранилище не отвечает: остальные вызовы только копили бы таймауты.
                    stopped = Some(error);
                    break 'sweep;
                }
            }
        }
        match stopped {
            // Запись удалена, но хвосты могли остаться: человек должен об этом узнать.
            Some(error) => Err(self.map(key, error)),
            None => Ok(()),
        }
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
        /// Сколько изменяющих вызовов ещё пройдёт до «падения»; `None` — без ограничений.
        pub budget: Mutex<Option<usize>>,
        /// Сколько изменяющих вызовов было (записи и удаления).
        pub changes: std::sync::atomic::AtomicUsize,
    }

    impl MemorySlots {
        /// Процесс «умирает» после `n` изменений: все следующие вызовы отказывают.
        pub fn crash_after(&self, n: usize) {
            *self.budget.lock().unwrap() = Some(n);
        }

        pub fn heal(&self) {
            *self.budget.lock().unwrap() = None;
        }

        fn spend(&self) -> Result<(), SlotError> {
            self.changes.fetch_add(1, Ordering::SeqCst);
            let mut budget = self.budget.lock().unwrap();
            match budget.as_mut() {
                Some(0) => Err(SlotError::Unavailable("process died".into())),
                Some(left) => {
                    *left -= 1;
                    Ok(())
                }
                None => Ok(()),
            }
        }

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
            self.spend()?;
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
            self.spend()?;
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
        backend.slots.store.lock().unwrap().remove("k#g1#1");
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
            .insert("k#g1#1".into(), "b".repeat(900));
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

    /// Вторая сторона: после «падения» на каждом шаге перезаписи читается старое значение
    /// целиком или новое целиком, а `delete` потом не оставляет ни одного куска.
    fn crash_matrix(prepare: impl Fn(&ChunkedBackend<MemorySlots>) -> String, new: &str) {
        let probe = windows_like();
        let old = prepare(&probe);
        probe.slots.changes.store(0, Ordering::SeqCst);
        probe.set("k", &Secret::new(new)).unwrap();
        let total = probe.slots.changes.load(Ordering::SeqCst);
        assert!(total >= 1);
        for step in 0..=total {
            let backend = windows_like();
            assert_eq!(prepare(&backend), old);
            backend.slots.crash_after(step);
            let _ = backend.set("k", &Secret::new(new));
            backend.slots.heal();
            let got = backend
                .get("k")
                .unwrap_or_else(|e| panic!("step {step}: unreadable after a crash: {e}"))
                .unwrap_or_else(|| panic!("step {step}: the value vanished"));
            assert!(
                got.expose() == old || got.expose() == new,
                "step {step}: neither the old nor the new value"
            );
            backend.delete("k").unwrap();
            assert_eq!(
                entries(&backend),
                0,
                "step {step}: delete left fragments: {:?}",
                backend
                    .slots
                    .store
                    .lock()
                    .unwrap()
                    .keys()
                    .collect::<Vec<_>>()
            );
        }
    }

    fn put(long: usize) -> impl Fn(&ChunkedBackend<MemorySlots>) -> String {
        move |backend| {
            let value = "o".repeat(long);
            backend.set("k", &Secret::new(value.clone())).unwrap();
            value
        }
    }

    #[test]
    fn a_crash_while_a_long_value_is_replaced_by_a_short_one_never_loses_the_old_value() {
        // Старое — пять кусков, новое — два: раньше запись `k#1` затирала часть старого.
        crash_matrix(put(4_300), &"n".repeat(1_500));
    }

    #[test]
    fn a_crash_while_a_short_value_is_replaced_by_a_long_one_is_safe() {
        crash_matrix(put(100), &"n".repeat(5_000));
    }

    #[test]
    fn a_crash_while_a_long_value_is_replaced_by_another_long_one_is_safe() {
        crash_matrix(put(3_000), &"n".repeat(3_500));
    }

    #[test]
    fn a_value_written_in_the_first_format_survives_a_crash_during_its_replacement() {
        let v1 = |backend: &ChunkedBackend<MemorySlots>| {
            let value = "v".repeat(2_500);
            let chunks = split_chunks(&value);
            let mut store = backend.slots.store.lock().unwrap();
            store.insert(
                "k".into(),
                format!("psec1:{}:{}:{}", chunks.len(), checksum(&value), chunks[0]),
            );
            for (i, chunk) in chunks.iter().enumerate().skip(1) {
                store.insert(format!("k#{i}"), (*chunk).to_string());
            }
            value
        };
        crash_matrix(v1, &"n".repeat(900));
    }

    #[test]
    fn delete_finds_the_tail_of_a_write_whose_header_never_landed() {
        let backend = windows_like();
        backend.set("k", &Secret::new("short")).unwrap();
        // Перезапись длинным значением упала после двух кусков и до заголовка.
        backend.slots.crash_after(2);
        let _ = backend.set("k", &Secret::new("x".repeat(4_000)));
        backend.slots.heal();
        assert!(entries(&backend) > 1, "the orphaned parts are there");
        backend.delete("k").unwrap();
        assert_eq!(entries(&backend), 0);
    }

    #[test]
    fn delete_sweeps_even_when_the_header_cannot_be_read() {
        let backend = windows_like();
        {
            let mut store = backend.slots.store.lock().unwrap();
            store.insert("k".into(), "psec2:garbage".into());
            store.insert("k#g1#1".into(), "tail".into());
            store.insert("k#1".into(), "old tail".into());
        }
        backend.delete("k").unwrap();
        assert_eq!(entries(&backend), 0);
    }

    #[test]
    fn a_dead_store_does_not_make_delete_wait_on_every_slot() {
        let backend = windows_like();
        backend.set("k", &Secret::new("x".repeat(3_000))).unwrap();
        backend.slots.crash_after(1);
        backend.slots.changes.store(0, Ordering::SeqCst);
        let _ = backend.delete("k");
        assert!(
            backend.slots.changes.load(Ordering::SeqCst) <= 3,
            "the sweep stops at the first failure"
        );
    }
}
