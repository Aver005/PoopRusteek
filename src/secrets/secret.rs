//! Значение секрета. Нигде не печатается: нет `Display`, `Debug` скрыт.

use sha2::{Digest, Sha256};

/// Секретная строка. Чтение только явным `expose()`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Единственная дверь к значению. Звать там, где оно уходит по назначению.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Первые 8 hex SHA-256: «ключ …a1b2» без раскрытия значения.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.0.as_bytes());
        digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
    }

    /// Сравнение без раннего выхода: время не зависит от места расхождения.
    pub fn matches(&self, other: &Secret) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        let mut diff = a.len() ^ b.len();
        for i in 0..a.len().max(b.len()) {
            diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
        }
        diff == 0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<secret>")
    }
}

impl Drop for Secret {
    /// Обнуляет сам буфер значения, а не его замену: `into_bytes` отдаёт ту же память, `fill`
    /// пишет в неё нули, `black_box` не даёт компилятору выбросить запись. Копии, которые
    /// ушли в заголовки и сокеты, этим не достать, и стопроцентной гарантии нет.
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_value() {
        let secret = Secret::new("sk-canary-123");
        assert_eq!(format!("{secret:?}"), "<secret>");
        assert_eq!(format!("{:?}", Some(&secret)), "Some(<secret>)");
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        let a = Secret::new("abc");
        assert_eq!(a.fingerprint(), Secret::new("abc").fingerprint());
        assert_ne!(a.fingerprint(), Secret::new("abd").fingerprint());
        assert_eq!(a.fingerprint().len(), 8);
    }

    #[test]
    fn matches_compares_whole_values() {
        assert!(Secret::new("abc").matches(&Secret::new("abc")));
        assert!(!Secret::new("abc").matches(&Secret::new("abcd")));
        assert!(!Secret::new("abc").matches(&Secret::new("abd")));
    }
}
