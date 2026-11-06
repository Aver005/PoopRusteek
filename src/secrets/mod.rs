//! Единое хранение секретов.
//!
//! Значение лежит в системном хранилище (`keyring`), в конфиге — ссылка
//! (`keyring:global/x`, `env:NAME`, `cmd:[…]`, `file:/путь`, `none`). Тип
//! [`Secret`] нигде не печатается. Это защита от копии файла, бэкапа и
//! случайного коммита, но не от процесса того же пользователя: агент с `bash`
//! при желании дотянется до хранилища так же, как до обычного файла.

pub mod backend;
pub mod chunked;
pub mod error;
pub mod keyring_slots;
pub mod ops;
pub mod redact;
pub mod reference;
pub mod resolver;
pub mod secret;
pub mod store;

pub use error::SecretError;
pub use reference::{RefKind, keyring_ref, looks_like_reference};
pub use resolver::{SecretResolver, StoreResolver, TrustContext};
pub use secret::Secret;
pub use store::{SecretStore, process_store};

/// Запись токена DeepSeek (`[provider] token`).
pub const DEEPSEEK_TOKEN_ID: &str = "global/deepseek-token";
/// Запись ключа локального сервера (`[server] api_key`).
pub const SERVER_KEY_ID: &str = "global/server-api-key";

/// Идентификатор записи для ключа аккаунта или `[[providers]]`: `global/<slug>`.
/// Детерминирован, чтобы повторный запуск миграции попадал в ту же запись.
pub fn account_secret_id(name: &str) -> String {
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let changed = slug != name;
    if slug.is_empty() {
        slug.push_str("account");
    }
    let reserved = matches!(slug.as_str(), "deepseek-token" | "server-api-key");
    if changed || reserved {
        use sha2::{Digest, Sha256};
        let tag: String = Sha256::digest(name.as_bytes())
            .iter()
            .take(2)
            .map(|b| format!("{b:02x}"))
            .collect();
        slug = format!("{slug}-{tag}");
    }
    format!("global/{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_map_to_readable_ids() {
        assert_eq!(account_secret_id("lmstudio"), "global/lmstudio");
        assert_eq!(account_secret_id("opencode-work"), "global/opencode-work");
    }

    #[test]
    fn unusual_names_get_a_stable_distinguishing_tag() {
        let a = account_secret_id("My Server");
        let b = account_secret_id("my_server");
        assert_ne!(a, b, "different names must not share a secret");
        assert_eq!(a, account_secret_id("My Server"));
        assert!(RefKind::parse(&keyring_ref(&a)).is_ok(), "{a}");
    }

    #[test]
    fn reserved_names_do_not_collide_with_fixed_ids() {
        assert_ne!(account_secret_id("deepseek-token"), DEEPSEEK_TOKEN_ID);
        assert_ne!(account_secret_id("server-api-key"), SERVER_KEY_ID);
    }
}
