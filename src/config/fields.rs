//! Поля конфига, в которых бывает секрет, и работа с ними на уровне TOML-текста.
//! Ничего не парсит в `Config`: миграция должна видеть файл таким, какой он есть.

use crate::secrets::{self, Secret, looks_like_reference};
use toml_edit::{DocumentMut, Item, TableLike, Value};

/// Пометка, которой затирается значение в копии файла.
pub const SCRUBBED: &str = "<moved-to-keyring>";

/// Какое именно поле держит секрет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotKind {
    DeepseekToken,
    ServerKey,
    /// Запись `[[providers]]` по имени.
    ProviderKey(String),
    /// Аккаунт `[[accounts]]` по id; наследие, ключ лежит в `api_key`.
    AccountKey(String),
    /// Ссылка `secret = "…"` аккаунта.
    AccountSecret(String),
    /// Ссылка `direct_token = "…"` аккаунта.
    AccountDirectToken(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub kind: SlotKind,
}

impl Slot {
    /// Для сообщений: путь поля без значения.
    pub fn label(&self) -> String {
        match &self.kind {
            SlotKind::DeepseekToken => "provider.token".to_string(),
            SlotKind::ServerKey => "server.api_key".to_string(),
            SlotKind::ProviderKey(name) => format!("providers[{name}].api_key"),
            SlotKind::AccountKey(id) => format!("accounts[{id}].api_key"),
            SlotKind::AccountSecret(id) => format!("accounts[{id}].secret"),
            SlotKind::AccountDirectToken(id) => format!("accounts[{id}].direct_token"),
        }
    }

    /// Запись в хранилище. Запись провайдера и его зеркало-аккаунт делят одну.
    pub fn secret_id(&self) -> String {
        match &self.kind {
            SlotKind::DeepseekToken => secrets::DEEPSEEK_TOKEN_ID.to_string(),
            SlotKind::ServerKey => secrets::SERVER_KEY_ID.to_string(),
            SlotKind::ProviderKey(name)
            | SlotKind::AccountKey(name)
            | SlotKind::AccountSecret(name)
            | SlotKind::AccountDirectToken(name) => secrets::account_secret_id(name),
        }
    }
}

/// Таблицы массива, записанного как `[[x]]` или как `x = [{…}]`.
fn tables_mut(item: &mut Item) -> Vec<&mut dyn TableLike> {
    match item {
        Item::ArrayOfTables(array) => array
            .iter_mut()
            .map(|table| table as &mut dyn TableLike)
            .collect(),
        Item::Value(Value::Array(array)) => array
            .iter_mut()
            .filter_map(|value| value.as_inline_table_mut().map(|t| t as &mut dyn TableLike))
            .collect(),
        _ => Vec::new(),
    }
}

fn text_of(item: Option<&Item>) -> Option<&str> {
    item.and_then(Item::as_str)
}

/// Проход по всем полям-кандидатам; `visit` видит поле и таблицу-владельца.
fn walk(doc: &mut DocumentMut, mut visit: impl FnMut(Slot, &mut dyn TableLike, &str)) {
    if let Some(table) = doc.get_mut("provider").and_then(Item::as_table_like_mut) {
        visit(
            Slot {
                kind: SlotKind::DeepseekToken,
            },
            table,
            "token",
        );
    }
    if let Some(table) = doc.get_mut("server").and_then(Item::as_table_like_mut) {
        visit(
            Slot {
                kind: SlotKind::ServerKey,
            },
            table,
            "api_key",
        );
    }
    if let Some(item) = doc.get_mut("providers") {
        for table in tables_mut(item) {
            let Some(name) = text_of(table.get("name")).map(str::to_string) else {
                continue;
            };
            visit(
                Slot {
                    kind: SlotKind::ProviderKey(name),
                },
                table,
                "api_key",
            );
        }
    }
    if let Some(item) = doc.get_mut("accounts") {
        for table in tables_mut(item) {
            let Some(id) = text_of(table.get("id")).map(str::to_string) else {
                continue;
            };
            visit(
                Slot {
                    kind: SlotKind::AccountKey(id.clone()),
                },
                table,
                "api_key",
            );
            // Эти два поля по правилам хранят только ссылку; литерал в них — чужая ошибка,
            // но это всё равно ключ, и читать такой файл модели нельзя.
            visit(
                Slot {
                    kind: SlotKind::AccountSecret(id.clone()),
                },
                table,
                "secret",
            );
            visit(
                Slot {
                    kind: SlotKind::AccountDirectToken(id),
                },
                table,
                "direct_token",
            );
        }
    }
}

/// Поля аккаунта, где допустима любая известная ссылка, в том числе `none`.
fn holds_reference_only(kind: &SlotKind) -> bool {
    matches!(
        kind,
        SlotKind::AccountSecret(_) | SlotKind::AccountDirectToken(_)
    )
}

/// Открытые секреты в файле: непустые строки, не являющиеся ссылками.
/// Ссылка-обёртка `none` для legacy-полей не считается ссылкой: это обычная строка.
pub fn plaintext_slots(doc: &mut DocumentMut) -> Vec<(Slot, Secret)> {
    let mut found = Vec::new();
    walk(doc, |slot, table, key| {
        if let Some(value) = text_of(table.get(key))
            && !value.trim().is_empty()
            && value != SCRUBBED
            && !(if holds_reference_only(&slot.kind) {
                looks_like_reference(value)
            } else {
                is_reference(value)
            })
        {
            found.push((slot, Secret::new(value)));
        }
    });
    found
}

/// Ссылки, уже стоящие в файле: подпись поля и сама ссылка (не значение). `none` не в счёт.
pub fn reference_slots(doc: &mut DocumentMut) -> Vec<(Slot, String)> {
    let mut found = Vec::new();
    walk(doc, |slot, table, key| {
        if let Some(value) = text_of(table.get(key))
            && is_reference(value)
        {
            found.push((slot, value.trim().to_string()));
        }
    });
    found
}

/// Ссылка для legacy-полей: четыре известные приставки (`none` здесь обычный текст).
pub fn is_reference(raw: &str) -> bool {
    looks_like_reference(raw) && raw.trim() != "none"
}

/// Заменяет значение поля ссылкой. Для аккаунта убирает `api_key` и ставит `secret`.
pub fn replace_with_reference(doc: &mut DocumentMut, target: &Slot, reference: &str) {
    walk(doc, |slot, table, key| {
        if slot.kind != target.kind {
            return;
        }
        match &slot.kind {
            SlotKind::AccountKey(_) => {
                table.remove(key);
                table.insert("secret", toml_edit::value(reference));
            }
            _ => set_string(table, key, reference),
        }
    });
}

/// Пустое значение поля секрета для файла, который читает старый бинарь: токены становятся
/// пустой строкой, ключи записей `api_key` убираются совсем (пустой ключ ушёл бы заголовком).
pub fn blank(doc: &mut DocumentMut, target: &Slot) {
    walk(doc, |slot, table, key| {
        if slot.kind != target.kind {
            return;
        }
        match &slot.kind {
            SlotKind::DeepseekToken | SlotKind::ServerKey => set_string(table, key, ""),
            _ => {
                table.remove(key);
            }
        }
    });
}

/// Меняет строку, сохранив комментарии вокруг неё.
pub fn set_string(table: &mut dyn TableLike, key: &str, text: &str) {
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            let mut new = Value::from(text);
            *new.decor_mut() = decor;
            *old = new;
        }
        Some(slot) => *slot = toml_edit::value(text),
        None => {
            table.insert(key, toml_edit::value(text));
        }
    }
}

/// Заменяет значения секретов в тексте-копии на пометку: бэкап больше не хранит ключи.
pub fn scrub_plaintext(text: &str) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    let found = plaintext_slots(&mut doc);
    if found.is_empty() {
        return None;
    }
    for (slot, _) in &found {
        walk(&mut doc, |candidate, table, key| {
            if candidate.kind == slot.kind {
                set_string(table, key, SCRUBBED);
            }
        });
    }
    Some(doc.to_string())
}

/// Есть ли в тексте конфига открытые секреты. Нечитаемый текст считается опасным.
pub fn contains_inline_secrets(text: &str) -> bool {
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return true;
    };
    !plaintext_slots(&mut doc).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# мой конфиг
[provider]
token = "tok-secret-1234567890"   # DeepSeek

[server]
api_key = "keyring:global/server-api-key"

[[providers]]
name = "lmstudio"
api_key = "lm-key-abcdef"

[[providers]]
name = "local"
base_url = "http://localhost:1/v1"
"#;

    #[test]
    fn plaintext_and_references_are_told_apart() {
        let mut doc: DocumentMut = SAMPLE.parse().unwrap();
        let plain = plaintext_slots(&mut doc);
        let labels: Vec<String> = plain.iter().map(|(s, _)| s.label()).collect();
        assert_eq!(labels, ["provider.token", "providers[lmstudio].api_key"]);
        let refs = reference_slots(&mut doc);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "keyring:global/server-api-key");
    }

    #[test]
    fn replacing_keeps_comments_and_the_rest_of_the_file() {
        let mut doc: DocumentMut = SAMPLE.parse().unwrap();
        let (slot, _) = plaintext_slots(&mut doc).remove(0);
        replace_with_reference(&mut doc, &slot, "keyring:global/deepseek-token");
        let text = doc.to_string();
        assert!(text.contains("# мой конфиг"), "{text}");
        assert!(text.contains("# DeepSeek"), "{text}");
        assert!(
            text.contains("token = \"keyring:global/deepseek-token\""),
            "{text}"
        );
        assert!(text.contains("lm-key-abcdef"), "other secrets untouched");
    }

    #[test]
    fn an_account_key_becomes_a_secret_reference() {
        let mut doc: DocumentMut = "[[accounts]]\nid = \"a\"\nprovider = \"openai\"\napi_key = \"sk-plain-value-000000\"\n"
            .parse()
            .unwrap();
        let (slot, value) = plaintext_slots(&mut doc).remove(0);
        assert_eq!(value.expose(), "sk-plain-value-000000");
        replace_with_reference(&mut doc, &slot, "keyring:global/a");
        let text = doc.to_string();
        assert!(!text.contains("api_key"), "{text}");
        assert!(text.contains("secret = \"keyring:global/a\""), "{text}");
    }

    /// Литерал в `secret` и `direct_token` аккаунта — тоже ключ: файл читать модели нельзя,
    /// а копию он должен затирать. `secret = "none"` и ссылки секретом не считаются.
    #[test]
    fn a_literal_in_an_accounts_secret_or_direct_token_counts_as_a_key() {
        let mut doc: DocumentMut = "[[accounts]]
id = \"a\"
provider = \"openai\"
secret = \"sk-literal-abcdef012345\"
direct_token = \"tok-literal-0123456\"
"
        .parse()
        .unwrap();
        let labels: Vec<String> = plaintext_slots(&mut doc)
            .iter()
            .map(|(s, _)| s.label())
            .collect();
        assert_eq!(labels, ["accounts[a].secret", "accounts[a].direct_token"]);
        assert!(contains_inline_secrets(&doc.to_string()));
        let scrubbed = scrub_plaintext(&doc.to_string()).unwrap();
        assert!(!scrubbed.contains("literal"), "{scrubbed}");
        for fine in [
            "[[accounts]]
id = \"a\"
provider = \"openai\"
secret = \"none\"
",
            "[[accounts]]
id = \"a\"
provider = \"openai\"
secret = \"keyring:global/a\"
direct_token = \"env:T\"
",
        ] {
            assert!(!contains_inline_secrets(fine), "{fine}");
        }
    }

    #[test]
    fn inline_array_tables_are_found_too() {
        let mut doc: DocumentMut =
            "providers = [{ name = \"x\", api_key = \"plain-key-123456\" }]\n"
                .parse()
                .unwrap();
        assert_eq!(plaintext_slots(&mut doc).len(), 1);
    }

    #[test]
    fn scrubbing_a_backup_removes_the_values_but_keeps_the_structure() {
        let scrubbed = scrub_plaintext(SAMPLE).unwrap();
        assert!(!scrubbed.contains("tok-secret"), "{scrubbed}");
        assert!(!scrubbed.contains("lm-key-abcdef"), "{scrubbed}");
        assert!(scrubbed.contains("# мой конфиг"));
        assert!(scrub_plaintext("[provider]\ntoken = \"keyring:global/x\"\n").is_none());
    }

    #[test]
    fn a_broken_text_counts_as_holding_secrets() {
        assert!(contains_inline_secrets("[provider\ntoken = \"x\""));
        assert!(!contains_inline_secrets("[ui]\ntheme = \"default\"\n"));
        assert!(contains_inline_secrets("[provider]\ntoken = \"abc\"\n"));
    }
}
