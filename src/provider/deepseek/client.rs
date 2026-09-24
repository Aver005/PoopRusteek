//! Кем мы представляемся DeepSeek. Сервер режет старые клиенты по этим
//! заголовкам (`40005 CLIENT_VERSION_TOO_LOW`), поэтому они сверены с живым вебом.

use reqwest::header::{HeaderMap, HeaderValue};

/// Версия веб-клиента chat.deepseek.com на 2026-09-24. От версии зависит и
/// форма ответов, поэтому её не выдумывают, а снимают из DevTools.
pub const DEFAULT_CLIENT_VERSION: &str = "2.5.0";
/// Порога для веба до авторизации не нашлось (перебор протухшим токеном,
/// 2026-09-24); у `android` мы стояли ровно на нём (1.8.0).
const PLATFORM: &str = "web";
const BUNDLE_ID: &str = "com.deepseek.chat";
const LOCALE: &str = "en_US";

/// Код отказа, которым сервер встречает слишком старый клиент.
const CLIENT_VERSION_TOO_LOW: i64 = 40005;
/// Код отказа для протухшего или чужого токена.
const INVALID_TOKEN: i64 = 40003;

/// Версия для заголовка: из `[provider] client_version`, иначе встроенная.
/// Негодное значение не валит провайдера: иначе приложение уходит на экран
/// токена с ложной причиной, а встроенная версия хотя бы честно попробует.
pub(super) fn client_version(configured: Option<&str>) -> HeaderValue {
    let default = HeaderValue::from_static(DEFAULT_CLIENT_VERSION);
    let Some(version) = configured.map(str::trim).filter(|v| !v.is_empty()) else {
        return default;
    };
    HeaderValue::from_str(version).unwrap_or_else(|_| {
        tracing::warn!(
            "[provider] client_version = {version:?} is not a valid header value; using {DEFAULT_CLIENT_VERSION}"
        );
        default
    })
}

/// Заголовки клиента, которыми представляется веб DeepSeek.
pub(super) fn insert_identity(headers: &mut HeaderMap, version: &HeaderValue) {
    headers.insert("x-client-platform", HeaderValue::from_static(PLATFORM));
    headers.insert("x-client-version", version.clone());
    headers.insert("x-client-bundle-id", HeaderValue::from_static(BUNDLE_ID));
    headers.insert("x-client-locale", HeaderValue::from_static(LOCALE));
}

/// Что делать пользователю при известном коде отказа. Сырой `40005` ничего
/// не говорит тому, кто просто хотел задать вопрос.
pub(super) fn refusal_hint(code: i64) -> Option<&'static str> {
    match code {
        CLIENT_VERSION_TOO_LOW => Some(
            "DeepSeek raised the minimum client version. Run /update; if that doesn't help, set client_version under [provider] in config.toml to the x-client-version chat.deepseek.com sends (DevTools → Network)",
        ),
        INVALID_TOKEN => Some(
            "the token is invalid or expired. Log in at chat.deepseek.com again, then /logout and paste the fresh userToken",
        ),
        _ => None,
    }
}

/// Сообщение отказа с подсказкой, если код знакомый.
pub(super) fn with_hint(message: String, code: Option<i64>) -> String {
    match code.and_then(refusal_hint) {
        Some(hint) => format!("{message} — {hint}"),
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пустое или пробельное значение в конфиге — это «не задано», а не
    /// пустой заголовок, который сервер примет за версию 0.
    #[test]
    fn a_blank_override_falls_back_to_the_built_in_version() {
        assert_eq!(client_version(None), DEFAULT_CLIENT_VERSION);
        assert_eq!(client_version(Some("  ")), DEFAULT_CLIENT_VERSION);
        assert_eq!(client_version(Some(" 2.6.1 ")), "2.6.1");
    }

    #[test]
    fn an_unusable_override_falls_back_instead_of_breaking_the_provider() {
        assert_eq!(client_version(Some("2.6\n.0")), DEFAULT_CLIENT_VERSION);
    }

    #[test]
    fn known_codes_carry_a_hint_and_unknown_ones_stay_as_is() {
        let version = with_hint("CLIENT_VERSION_TOO_LOW (code 40005)".into(), Some(40005));
        assert!(version.contains("client_version"), "{version}");
        let token = with_hint("invalid token (code 40003)".into(), Some(40003));
        assert!(token.contains("/logout"), "{token}");
        assert_eq!(with_hint("x".into(), Some(40300)), "x");
        assert_eq!(with_hint("x".into(), None), "x");
    }
}
