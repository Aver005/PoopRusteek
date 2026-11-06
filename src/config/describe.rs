//! Ошибки разбора TOML без значений.
//!
//! `toml::de::Error` в `Display` печатает строку исходника вместе со значением, а в
//! тексте serde «invalid type: string "…"» стоит само значение. Ключ в неверном поле
//! не должен попадать в терминал и журнал, поэтому берём `message()` и позицию.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

fn value_echo() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(invalid (?:type|value): (?:string|integer|floating point|boolean))[^,]*"#)
            .expect("pattern is valid")
    })
}

/// Строка и столбец (с единицы) для байтового смещения. Считает символы, не байты.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[..end];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        + 1;
    (line, column)
}

pub fn describe(path: &Path, text: &str, error: &toml::de::Error) -> String {
    let message = value_echo().replace_all(error.message(), "$1").into_owned();
    match error.span() {
        Some(span) => {
            let (line, column) = line_col(text, span.start);
            format!(
                "{} (line {line}, column {column}): {message}",
                path.display()
            )
        }
        None => format!("{}: {message}", path.display()),
    }
}

/// Для `toml_edit`: тот же принцип.
pub fn describe_edit(path: &Path, text: &str, error: &toml_edit::TomlError) -> String {
    match error.span() {
        Some(span) => {
            let (line, column) = line_col(text, span.start);
            format!(
                "{} (line {line}, column {column}): {}",
                path.display(),
                error.message()
            )
        }
        None => format!("{}: {}", path.display(), error.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Probe {
        retries: Option<i32>,
    }

    #[test]
    fn an_unknown_key_error_names_the_place_but_not_the_value() {
        let text = "retriez = \"sk-live-canary-9999\"\n";
        let error = toml::from_str::<Probe>(text).unwrap_err();
        let shown = describe(Path::new("c.toml"), text, &error);
        assert!(shown.contains("line 1"), "{shown}");
        assert!(!shown.contains("canary"), "{shown}");
    }

    #[test]
    fn a_wrong_type_does_not_echo_the_offending_value() {
        let text = "retries = \"sk-live-canary-9999\"\n";
        let error = toml::from_str::<Probe>(text).unwrap_err();
        let shown = describe(Path::new("c.toml"), text, &error);
        assert!(!shown.contains("canary"), "{shown}");
        assert!(shown.contains("invalid type"), "{shown}");
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        let text = "# ключ\nretriez = 1\n";
        let error = toml::from_str::<Probe>(text).unwrap_err();
        let shown = describe(Path::new("c.toml"), text, &error);
        assert!(shown.contains("line 2, column 1"), "{shown}");
    }
}
