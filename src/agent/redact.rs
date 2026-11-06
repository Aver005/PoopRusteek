//! Маскировка секретов в коротких строках для экрана (панель работы). Простая
//! копия правил будущего `secrets/redact.rs`; ведущий потом объединит их.

use regex::Regex;
use std::sync::LazyLock;

/// Чем заменяется значение.
const MASK: &str = "***";

/// Части имён полей и флагов, за которыми прячется секрет.
const SECRET_WORDS: [&str; 7] = [
    "token", "api_key", "apikey", "api-key", "password", "passwd", "secret",
];

/// Имя поля или флага похоже на секрет (`access_token`, `Authorization`).
pub(crate) fn is_secret_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("authorization") || SECRET_WORDS.iter().any(|word| lower.contains(word))
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("hardcoded regex is valid")
}

/// `Bearer abc`, `Authorization: Basic abc`.
static BEARER: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\bbearer\s+[^\s'\x22]+"));
static AUTH_HEADER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(authorization\s*[:=]\s*)(?:basic|bearer|token)?\s*[^\s'\x22]+"));
/// `--token abc`, `--password=abc`, `-p` не трогаем: короткие флаги неоднозначны.
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)(--[\w-]*(?:token|password|passwd|secret|api[-_]?key|authorization)[\w-]*)(\s+|=)[^\s'\x22]+",
    )
});
/// `API_KEY=abc`, `token=abc`, `"password": "abc"`.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)(["']?[\w-]*(?:token|password|passwd|secret|api[-_]?key)[\w-]*["']?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,;]+)"#,
    )
});

/// Заменить значения секретов маской: токены в заголовках, флагах, присваиваниях.
pub(crate) fn redact_secrets(text: &str) -> String {
    let text = BEARER.replace_all(text, format!("Bearer {MASK}"));
    let text = AUTH_HEADER.replace_all(&text, format!("${{1}}{MASK}"));
    let text = FLAG.replace_all(&text, format!("${{1}}${{2}}{MASK}"));
    ASSIGNMENT
        .replace_all(&text, format!("${{1}}{MASK}"))
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_looking_names_are_recognised() {
        for name in [
            "token",
            "access_token",
            "API_KEY",
            "x-api-key",
            "password",
            "Authorization",
            "client_secret",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["command", "path", "query", "name"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn bearer_tokens_and_auth_headers_are_masked() {
        let masked = redact_secrets("curl -H 'Authorization: Bearer abc.def' https://x");
        assert!(!masked.contains("abc.def"), "{masked}");
        assert!(redact_secrets("Bearer zzz123").contains("Bearer ***"));
        assert!(!redact_secrets("authorization: Basic dXNlcjpwYXNz").contains("dXNlcjpwYXNz"));
    }

    #[test]
    fn secret_flags_are_masked_with_a_space_or_an_equals_sign() {
        for command in [
            "deploy --token s3cr3t now",
            "deploy --token=s3cr3t now",
            "login --password hunter2",
            "login --password=hunter2",
            "run --api-key sk-12345",
        ] {
            let masked = redact_secrets(command);
            assert!(masked.contains("***"), "{masked}");
            for secret in ["s3cr3t", "hunter2", "sk-12345"] {
                assert!(!masked.contains(secret), "{masked}");
            }
        }
    }

    #[test]
    fn env_style_assignments_are_masked_but_plain_ones_are_not() {
        let masked = redact_secrets("API_KEY=abc123 SECRET_TOKEN=xyz cargo run");
        assert!(
            !masked.contains("abc123") && !masked.contains("xyz"),
            "{masked}"
        );
        assert!(masked.contains("cargo run"));
        assert_eq!(
            redact_secrets("RUST_LOG=debug cargo test"),
            "RUST_LOG=debug cargo test"
        );
    }

    #[test]
    fn json_like_fields_are_masked() {
        let masked = redact_secrets(r#"{"password": "hunter2", "user": "bob"}"#);
        assert!(!masked.contains("hunter2"), "{masked}");
        assert!(masked.contains("bob"));
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        let text = "cargo test --bin pooprusteek items::range -- --nocapture";
        assert_eq!(redact_secrets(text), text);
    }
}
