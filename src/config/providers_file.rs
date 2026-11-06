//! Правка `[[providers]]` в файле при выключенном шлюзе: точечно, мимо сериализации конфига.
//! Там записи только читаются, а ключ в файл должен попадать ссылкой, не значением.

use super::write::{self, Op, Seg};
use super::{ProviderEntry, parse};
use crate::error::{AppError, AppResult};
use std::path::Path;

fn providers_path() -> Vec<Seg> {
    vec![Seg::Key("providers".into())]
}

fn edit(path: &Path, ops: impl FnOnce(&str) -> Result<Vec<Op>, String>) -> AppResult<()> {
    write::with_document(path, |doc| {
        let ops = ops(&doc.to_string())?;
        write::apply(doc, &ops);
        Ok(())
    })
    .map(|_| ())
    .map_err(|error| AppError::Config(error.to_string()))
}

/// Добавить запись; ключ в файл идёт ссылкой `key_reference`, а не значением.
pub fn add_provider_at(
    path: &Path,
    entry: &ProviderEntry,
    key_reference: Option<&str>,
) -> AppResult<()> {
    let mut stored = entry.clone();
    stored.api_key = key_reference.map(str::to_string);
    let value =
        toml::Value::try_from(&stored).map_err(|error| AppError::Config(error.to_string()))?;
    edit(path, |text| {
        let typed = parse(text).map_err(|error| error.message().to_string())?;
        if typed.providers.iter().any(|p| p.name == entry.name) {
            return Err(format!("'{}' already exists", entry.name));
        }
        Ok(vec![Op::Push(providers_path(), value)])
    })
}

/// Убрать запись (откат неудачного добавления).
pub fn remove_provider_at(path: &Path, name: &str) -> AppResult<()> {
    edit(path, |_| {
        Ok(vec![Op::RemoveElem(providers_path(), name.to_string())])
    })
}

/// Поменять строковое поле записи: `model`, `tools`.
pub fn set_provider_field_at(path: &Path, name: &str, key: &str, value: &str) -> AppResult<()> {
    edit(path, |text| {
        let typed = parse(text).map_err(|error| error.message().to_string())?;
        if !typed.providers.iter().any(|p| p.name == name) {
            return Err(format!("no provider '{name}' in the config file"));
        }
        Ok(vec![Op::Set(
            vec![
                Seg::Key("providers".into()),
                Seg::Elem(name.to_string()),
                Seg::Key(key.to_string()),
            ],
            toml::Value::String(value.to_string()),
        )])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProtocol;

    fn temp_file(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-providers-file-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn entry(name: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.into(),
            base_url: "http://localhost:1234/v1".into(),
            api_key: Some("plain-value-never-written".into()),
            model: "m1".into(),
            protocol: ProviderProtocol::Openai,
            tools: Default::default(),
        }
    }

    #[test]
    fn an_entry_is_added_next_to_the_old_ones_and_the_key_goes_in_as_a_reference() {
        let path = temp_file("add");
        std::fs::write(
            &path,
            "# мой файл\n[ui]\ntheme = \"x\"\n\n[[providers]]\nname = \"old\"\nbase_url = \"http://old/v1\"\nmodel = \"m\"\n",
        )
        .unwrap();
        add_provider_at(&path, &entry("lm"), Some("keyring:global/lm")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[[providers]]").count(), 2, "{text}");
        assert!(
            text.contains("# мой файл") && text.contains("name = \"old\""),
            "{text}"
        );
        assert!(text.contains("api_key = \"keyring:global/lm\""), "{text}");
        assert!(!text.contains("plain-value-never-written"), "{text}");
        let typed = parse(&text).unwrap();
        assert_eq!(typed.providers[1].name, "lm");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_taken_name_is_refused_and_the_file_stays_as_it_was() {
        let path = temp_file("dup");
        add_provider_at(&path, &entry("lm"), None).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let error = add_provider_at(&path, &entry("lm"), None).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_model_is_changed_in_place_and_a_removal_rolls_the_add_back() {
        let path = temp_file("edit");
        add_provider_at(&path, &entry("lm"), None).unwrap();
        add_provider_at(&path, &entry("other"), None).unwrap();
        set_provider_field_at(&path, "lm", "model", "m2").unwrap();
        let typed = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(typed.providers[0].model, "m2");
        assert_eq!(typed.providers[1].model, "m1", "the neighbour is untouched");
        assert!(set_provider_field_at(&path, "ghost", "model", "x").is_err());
        remove_provider_at(&path, "lm").unwrap();
        let typed = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(typed.providers.len(), 1);
        assert_eq!(typed.providers[0].name, "other");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
