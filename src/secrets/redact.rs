//! Одна функция вычёркивания секретов для всех стоков: журнал отладки, трассы,
//! заголовки. Знает точные значения, которые приложение читало, и
//! узнаёт типичные формы ключей, которых оно не видело.

use super::secret::Secret;
use regex::Regex;
use std::borrow::Cow;
use std::sync::{OnceLock, RwLock};

pub const MASK: &str = "<redacted>";
/// Короче этого значение не регистрируем: «abc» вычеркнуло бы пол-лога.
const MIN_REGISTERED_LEN: usize = 8;

static KNOWN: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Запомнить значение: с этого момента оно вычёркивается из всего, что идёт в логи.
pub fn register(secret: &Secret) {
    if secret.len() < MIN_REGISTERED_LEN {
        return;
    }
    if let Ok(mut known) = KNOWN.write()
        && !known.iter().any(|v| v == secret.expose())
    {
        known.push(secret.expose().to_string());
    }
}

/// Строка с вычеркнутыми секретами. Без секретов возвращает исходную без копии.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(text);
    if let Ok(known) = KNOWN.read() {
        for value in known.iter() {
            if out.contains(value.as_str()) {
                out = Cow::Owned(out.replace(value.as_str(), MASK));
            }
        }
    }
    let patterns = patterns();
    if patterns.iter().any(|(re, _)| re.is_match(&out)) {
        let mut owned = out.into_owned();
        for (re, replacement) in patterns {
            owned = re.replace_all(&owned, *replacement).into_owned();
        }
        out = Cow::Owned(owned);
    }
    out
}

/// Заголовок для журнала: значения опасных заголовков не показываем вовсе.
pub fn header_value(name: &str, value: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let sensitive = matches!(
        lower.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "x-goog-api-key"
            | "api-key"
    ) || lower.contains("token")
        || lower.contains("secret");
    if sensitive {
        MASK.to_string()
    } else {
        redact(value).into_owned()
    }
}

fn patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let build = |pattern: &str, replacement: &'static str| {
            (
                Regex::new(pattern).expect("redaction pattern is valid"),
                replacement,
            )
        };
        vec![
            build(r"\bsk-[A-Za-z0-9_\-]{16,}", MASK),
            build(r"\bgh[pousr]_[A-Za-z0-9]{20,}", MASK),
            build(r"\bAKIA[0-9A-Z]{16}\b", MASK),
            build(r"\bAIza[0-9A-Za-z_\-]{30,}", MASK),
            build(
                r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
                MASK,
            ),
            build(
                r"(?i)(bearer\s+)[A-Za-z0-9._~+/\-]{16,}=*",
                "${1}<redacted>",
            ),
        ]
    })
}

/// Похожа ли строка на ключ: для проверки проектного конфига и `doctor`.
pub fn looks_like_secret(text: &str) -> bool {
    patterns().iter().any(|(re, _)| re.is_match(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_are_replaced_everywhere() {
        register(&Secret::new("canary-known-value-1"));
        let line = redact("token=canary-known-value-1; again canary-known-value-1");
        assert!(!line.contains("canary-known-value-1"), "{line}");
        assert_eq!(line.matches(MASK).count(), 2);
    }

    #[test]
    fn short_values_are_not_registered() {
        register(&Secret::new("abc"));
        assert_eq!(redact("abc def"), "abc def");
    }

    #[test]
    fn common_key_shapes_are_recognised_without_registration() {
        for sample in [
            "sk-proj-abcdefghijklmnopqrstuvwx",
            "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "AKIAABCDEFGHIJKLMNOP",
            "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0NTY.SflKxwRJSMeKKF2QT4",
        ] {
            let line = format!("key={sample} end");
            let out = redact(&line);
            assert!(!out.contains(sample), "{sample} -> {out}");
        }
        let out = redact("Authorization: Bearer abcdefghijklmnopqrstuv0123");
        assert!(out.contains("Bearer <redacted>"), "{out}");
    }

    #[test]
    fn ordinary_text_is_untouched_and_not_copied() {
        let text = "ran cargo test, 12 passed; see sk-short and Bearer x";
        assert!(matches!(redact(text), Cow::Borrowed(_)));
    }

    #[test]
    fn dangerous_headers_are_hidden_completely() {
        assert_eq!(header_value("Authorization", "Bearer abc"), MASK);
        assert_eq!(header_value("X-Api-Key", "k"), MASK);
        assert_eq!(header_value("Cookie", "a=b"), MASK);
        assert_eq!(header_value("x-auth-token", "t"), MASK);
        assert_eq!(
            header_value("accept", "application/json"),
            "application/json"
        );
    }
}
