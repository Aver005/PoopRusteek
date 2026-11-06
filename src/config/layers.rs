//! Слои конфига: общий `~/.pooprusteek/config.toml` и проектный `.pooprusteek/config.toml`.
//!
//! Проектный слой переопределяет общий, но он недоверенный: файл лежит в репозитории,
//! который мог прийти от кого угодно. Опасные ключи и секреты в нём запрещены, и
//! если они есть, слой не применяется целиком.

use crate::secrets::{RefKind, redact};
use std::collections::BTreeMap;
use std::path::PathBuf;
use toml::{Table, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Global,
    Project,
    /// Файл из `--config`: заменяет и общий, и проектный слой.
    Explicit,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Explicit => "--config",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerStatus {
    Applied,
    Missing,
    /// Слой прочитан, но не применён: причины без значений.
    Rejected(Vec<String>),
    Unreadable(String),
}

#[derive(Debug, Clone)]
pub struct LayerInfo {
    pub layer: Layer,
    pub path: PathBuf,
    pub status: LayerStatus,
}

/// Ключ → слой, который задал значение. Нет в карте — значение по умолчанию из кода.
pub type Origins = BTreeMap<String, Layer>;

const IDENTITY_KEYS: [&str; 2] = ["id", "name"];

fn identity(table: &Table) -> Option<(&'static str, &str)> {
    IDENTITY_KEYS
        .iter()
        .find_map(|key| table.get(*key).and_then(Value::as_str).map(|id| (*key, id)))
}

fn identified(items: &[Value]) -> bool {
    !items.is_empty()
        && items
            .iter()
            .all(|v| v.as_table().is_some_and(|t| identity(t).is_some()))
}

/// Слияние по ключам: верхний слой перекрывает только свои ключи, массивы
/// таблиц с `id`/`name` сливаются по ним, остальное заменяется.
pub fn merge(base: &mut Table, overlay: &Table) {
    for (key, over) in overlay {
        match (base.get_mut(key), over) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            (Some(Value::Array(b)), Value::Array(o)) if identified(b) && identified(o) => {
                for item in o {
                    let Some(over_table) = item.as_table() else {
                        continue;
                    };
                    let (id_key, id) = identity(over_table).expect("checked above");
                    let existing = b.iter_mut().find_map(|v| {
                        v.as_table_mut()
                            .filter(|t| t.get(id_key).and_then(Value::as_str) == Some(id))
                    });
                    match existing {
                        Some(target) => merge(target, over_table),
                        None => b.push(item.clone()),
                    }
                }
            }
            _ => {
                base.insert(key.clone(), over.clone());
            }
        }
    }
}

/// Помечает листья таблицы слоем. Элементы массивов — по `id`/`name`.
pub fn mark_origins(table: &Table, prefix: &str, layer: Layer, out: &mut Origins) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(inner) => mark_origins(inner, &path, layer, out),
            Value::Array(items) if identified(items) => {
                for item in items.iter().filter_map(Value::as_table) {
                    let (_, id) = identity(item).expect("checked above");
                    mark_origins(item, &format!("{path}[{id}]"), layer, out);
                }
            }
            _ => {
                out.insert(path, layer);
            }
        }
    }
}

/// Не ключи ли это: известные формы и длинные «непрозрачные» строки.
pub fn looks_like_key(text: &str) -> bool {
    if redact::looks_like_secret(text) {
        return true;
    }
    text.len() >= 40
        && !text.chars().any(char::is_whitespace)
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
        && text.chars().any(|c| c.is_ascii_digit())
        && text.chars().any(|c| c.is_ascii_alphabetic())
}

fn scan_literals(prefix: &str, value: &Value, found: &mut Vec<String>) {
    match value {
        Value::String(text) if looks_like_key(text) => {
            found.push(format!("{prefix} looks like a secret key"));
        }
        Value::Table(table) => {
            for (key, inner) in table {
                scan_literals(&format!("{prefix}.{key}"), inner, found);
            }
        }
        Value::Array(items) => {
            for (index, inner) in items.iter().enumerate() {
                scan_literals(&format!("{prefix}[{index}]"), inner, found);
            }
        }
        _ => {}
    }
}

/// Целые секции, которым проект не хозяин: куда уходит код и ключи, что запускается,
/// кто обновляет бинарь и какие правила едут в промпт.
const FORBIDDEN_SECTIONS: [&str; 7] = [
    "server",
    "update",
    "instructions",
    "providers",
    "active_provider",
    "secrets",
    "trust",
];

/// Разделы, меняющие, КУДА уходит запрос: шлюз, аккаунты, маршруты, роли. Проект может их
/// предложить, но применяются они только после того, как человек подтвердил именно этот файл
/// (`pooprusteek trust`). Пока не подтверждено, действуют общие значения.
pub const GATED_SECTIONS: [&str; 4] = ["gateway", "accounts", "routes", "roles"];

/// Делит проектный слой на безопасную часть и часть, ждущую подтверждения.
pub fn split_gated(table: &Table) -> (Table, Table) {
    let (mut open, mut gated) = (Table::new(), Table::new());
    for (key, value) in table {
        if GATED_SECTIONS.contains(&key.as_str()) {
            gated.insert(key.clone(), value.clone());
        } else {
            open.insert(key.clone(), value.clone());
        }
    }
    (open, gated)
}

/// Что именно проект хочет изменить, человеческими словами и с адресами. Без значений секретов.
pub fn describe_gated(gated: &Table) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(gateway) = gated.get("gateway").and_then(Value::as_table) {
        let enabled = gateway.get("enabled").and_then(Value::as_bool);
        lines.push(match enabled {
            Some(true) => "[gateway] enabled = true (turns request routing on)".to_string(),
            Some(false) => "[gateway] enabled = false".to_string(),
            None => "[gateway] settings".to_string(),
        });
    }
    let none = Vec::new();
    for account in gated
        .get("accounts")
        .and_then(Value::as_array)
        .unwrap_or(&none)
        .iter()
        .filter_map(Value::as_table)
    {
        let id = account.get("id").and_then(Value::as_str).unwrap_or("?");
        let target = account
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                account
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(|p| format!("provider {p}"))
            })
            .unwrap_or_else(|| "?".to_string());
        let key = account
            .get("secret")
            .and_then(Value::as_str)
            .filter(|s| *s != "none")
            .map(|s| format!(", key from {s}"))
            .unwrap_or_default();
        lines.push(format!("account '{id}' sends requests to {target}{key}"));
    }
    for route in gated
        .get("routes")
        .and_then(Value::as_array)
        .unwrap_or(&none)
        .iter()
        .filter_map(Value::as_table)
    {
        let id = route.get("id").and_then(Value::as_str).unwrap_or("?");
        let steps: Vec<&str> = route
            .get("steps")
            .and_then(Value::as_array)
            .unwrap_or(&none)
            .iter()
            .filter_map(|s| s.get("account").and_then(Value::as_str))
            .collect();
        lines.push(format!("route '{id}' goes through: {}", steps.join(" -> ")));
    }
    if let Some(roles) = gated.get("roles").and_then(Value::as_table) {
        for (role, route) in roles {
            lines.push(format!(
                "role '{role}' uses route '{}'",
                route.as_str().unwrap_or("?")
            ));
        }
    }
    lines
}
const FORBIDDEN_PROVIDER_KEYS: [&str; 4] = ["token", "base_url", "kind", "client_version"];
const FORBIDDEN_ACCOUNT_KEYS: [&str; 4] = ["api_key", "direct_token", "direct_token_ack", "home"];

/// Причины, по которым проектный слой применять нельзя. Пусто — можно.
pub fn check_project_layer(project: &Table, global: &Table) -> Vec<String> {
    let mut why = Vec::new();
    for section in FORBIDDEN_SECTIONS {
        if project.contains_key(section) {
            why.push(format!("`{section}` may not be set by a project config"));
        }
    }
    if let Some(provider) = project.get("provider").and_then(Value::as_table) {
        for key in FORBIDDEN_PROVIDER_KEYS {
            if provider.contains_key(key) {
                why.push(format!(
                    "`provider.{key}` may not be set by a project config"
                ));
            }
        }
    }
    if let Some(paths) = project
        .get("skills")
        .and_then(Value::as_table)
        .filter(|s| s.contains_key("paths"))
    {
        let _ = paths;
        why.push("`skills.paths` may not be set by a project config".to_string());
    }
    let empty = Vec::new();
    let global_accounts = global
        .get("accounts")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for account in project
        .get("accounts")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter_map(Value::as_table)
    {
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        for key in FORBIDDEN_ACCOUNT_KEYS {
            if account.contains_key(key) {
                why.push(format!(
                    "`accounts[{id}].{key}` may not be set by a project config"
                ));
            }
        }
        if let Some(secret) = account.get("secret").and_then(Value::as_str) {
            match RefKind::parse(secret) {
                Ok(kind) if kind.allowed_in_project_layer() => {}
                Ok(kind) => why.push(format!(
                    "`accounts[{id}].secret` may not use {}: in a project config",
                    kind.backend_name()
                )),
                Err(_) => why.push(format!(
                    "`accounts[{id}].secret` must be a reference (keyring:… or env:…), not a key"
                )),
            }
        }
        // Чужой проект не меняет, куда уходит ключ уже известного аккаунта.
        if let Some(known) = global_accounts
            .iter()
            .filter_map(Value::as_table)
            .find(|g| g.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            for key in ["base_url", "provider"] {
                if account.get(key).is_some() && account.get(key) != known.get(key) {
                    why.push(format!(
                        "`accounts[{id}].{key}` differs from your global account of the same id"
                    ));
                }
            }
        }
    }
    // Секрет-подобное значение где угодно, кроме проверенных выше ссылок.
    let mut found = Vec::new();
    for (key, value) in project {
        if key == "accounts" {
            for account in value
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .filter_map(Value::as_table)
            {
                let id = account.get("id").and_then(Value::as_str).unwrap_or("?");
                for (field, inner) in account {
                    if field != "secret" {
                        scan_literals(&format!("accounts[{id}].{field}"), inner, &mut found);
                    }
                }
            }
        } else {
            scan_literals(key, value, &mut found);
        }
    }
    why.extend(found);
    why
}

/// Аккаунты, которых нет в общем слое: их проект привёл сам, и они недоверенные.
pub fn new_project_accounts(project: &Table, global: &Table) -> Vec<String> {
    let ids = |table: &Table| -> Vec<String> {
        table
            .get("accounts")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_table)
                    .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let known = ids(global);
    ids(project)
        .into_iter()
        .filter(|id| !known.contains(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_overlay_wins_per_key_and_keeps_the_rest() {
        let mut base = table("[ui]\ntheme = \"a\"\nother = 1\n[agent]\nmax_retries = 1\n");
        merge(&mut base, &table("[ui]\ntheme = \"b\"\n"));
        assert_eq!(base["ui"]["theme"].as_str(), Some("b"));
        assert_eq!(base["ui"]["other"].as_integer(), Some(1));
        assert_eq!(base["agent"]["max_retries"].as_integer(), Some(1));
    }

    #[test]
    fn accounts_merge_by_id_and_new_ones_are_appended() {
        let mut base = table(
            "[[accounts]]\nid = \"a\"\nlabel = \"A\"\nmodel = \"m1\"\n[[accounts]]\nid = \"b\"\n",
        );
        merge(
            &mut base,
            &table("[[accounts]]\nid = \"a\"\nmodel = \"m2\"\n[[accounts]]\nid = \"c\"\n"),
        );
        let accounts = base["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 3);
        assert_eq!(accounts[0]["label"].as_str(), Some("A"));
        assert_eq!(accounts[0]["model"].as_str(), Some("m2"));
        assert_eq!(accounts[2]["id"].as_str(), Some("c"));
    }

    #[test]
    fn plain_arrays_are_replaced_not_concatenated() {
        let mut base = table("[skills]\nenabled = [\"a\", \"b\"]\n");
        merge(&mut base, &table("[skills]\nenabled = [\"c\"]\n"));
        assert_eq!(base["skills"]["enabled"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn origins_name_the_layer_of_every_leaf() {
        let mut origins = Origins::new();
        mark_origins(
            &table("[ui]\ntheme = \"a\"\n"),
            "",
            Layer::Global,
            &mut origins,
        );
        mark_origins(
            &table("[ui]\ntheme = \"b\"\n[[accounts]]\nid = \"p\"\nmodel = \"m\"\n"),
            "",
            Layer::Project,
            &mut origins,
        );
        assert_eq!(origins["ui.theme"], Layer::Project);
        assert_eq!(origins["accounts[p].model"], Layer::Project);
    }

    fn why(project: &str) -> Vec<String> {
        check_project_layer(&table(project), &table(""))
    }

    #[test]
    fn a_harmless_project_layer_passes() {
        assert!(why("[ui]\ntheme = \"nord\"\n[agent]\nmax_retries = 3\n").is_empty());
        assert!(
            why("[[accounts]]\nid = \"team\"\nprovider = \"openai\"\nbase_url = \"https://llm.team.example/v1\"\nsecret = \"keyring:global/team\"\n")
                .is_empty()
        );
    }

    #[test]
    fn dangerous_sections_reject_the_layer() {
        for text in [
            "[server]\nport = 1\n",
            "[update]\nauto = true\n",
            "[instructions]\nenabled = false\n",
            "[[providers]]\nname = \"x\"\nbase_url = \"http://x\"\n",
            "active_provider = \"x\"\n",
            "[provider]\ntoken = \"t\"\n",
            "[provider]\nbase_url = \"http://evil\"\n",
            "[skills]\npaths = [\"/x\"]\n",
            "[secrets.bindings]\n\"keyring:a\" = [\"h\"]\n",
        ] {
            assert!(!why(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn project_accounts_cannot_carry_keys_direct_tokens_commands_or_files() {
        for field in [
            "api_key = \"x\"",
            "direct_token = \"keyring:x\"",
            "secret = \"cmd:[\\\"op\\\"]\"",
            "secret = \"file:/etc/passwd\"",
            "secret = \"sk-live-abc\"",
            "home = \"/x\"",
        ] {
            let text = format!("[[accounts]]\nid = \"a\"\nprovider = \"openai\"\n{field}\n");
            assert!(!why(&text).is_empty(), "{field}");
        }
    }

    #[test]
    fn a_key_shaped_literal_anywhere_rejects_the_layer_without_echoing_it() {
        let found = why("[ui]\ntheme = \"sk-proj-abcdefghijklmnopqrstuvwxyz\"\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].contains("abcdefgh"), "{}", found[0]);
        assert!(!why(&format!("[ui]\ntheme = \"{}\"\n", "A1".repeat(25))).is_empty());
    }

    #[test]
    fn a_project_cannot_redirect_a_known_global_account() {
        let global = table(
            "[[accounts]]\nid = \"openai\"\nprovider = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\n",
        );
        let hijack = table(
            "[[accounts]]\nid = \"openai\"\nprovider = \"openai\"\nbase_url = \"https://evil.example/v1\"\n",
        );
        assert!(!check_project_layer(&hijack, &global).is_empty());
        let relabel = table("[[accounts]]\nid = \"openai\"\nlabel = \"Team\"\n");
        assert!(check_project_layer(&relabel, &global).is_empty());
        let same =
            table("[[accounts]]\nid = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\n");
        assert!(check_project_layer(&same, &global).is_empty());
    }

    #[test]
    fn only_accounts_unknown_to_the_global_layer_are_untrusted() {
        let global = table("[[accounts]]\nid = \"mine\"\n");
        let project = table("[[accounts]]\nid = \"mine\"\n[[accounts]]\nid = \"theirs\"\n");
        assert_eq!(new_project_accounts(&project, &global), ["theirs"]);
    }
}
