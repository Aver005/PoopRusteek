//! Точечная запись конфига.
//!
//! Структуру читаем, но не пишем целиком: сравниваем «каким конфиг загрузили» и
//! «каким он стал» и переносим в файл только разницу через `toml_edit`. Комментарии,
//! порядок и незнакомые ключи остаются. Повреждённый вручную файл не затираем.

use super::describe::{describe, describe_edit};
use super::lock::FileLock;
use std::path::Path;
use toml::{Table, Value};
use toml_edit::{DocumentMut, Item, TableLike};

/// Шаг пути. `Elem` стоит сразу после ключа массива и выбирает таблицу по `id`/`name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Elem(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Set(Vec<Seg>, Value),
    Remove(Vec<Seg>),
    /// Добавить таблицу в массив по пути.
    Push(Vec<Seg>, Value),
    RemoveElem(Vec<Seg>, String),
}

const IDENTITY_KEYS: [&str; 2] = ["id", "name"];

fn identity_of(value: &Value, key: &str) -> Option<String> {
    value
        .as_table()
        .and_then(|t| t.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Ключ, по которому элементы массива различимы; `None` — массив значений без личности.
fn identity_key(items: &[Value]) -> Option<&'static str> {
    IDENTITY_KEYS.iter().copied().find(|key| {
        let mut seen = std::collections::BTreeSet::new();
        items
            .iter()
            .all(|v| identity_of(v, key).is_some_and(|id| seen.insert(id)))
    })
}

fn extend(path: &[Seg], seg: Seg) -> Vec<Seg> {
    let mut next = path.to_vec();
    next.push(seg);
    next
}

/// Разница двух состояний одной и той же структуры.
pub fn diff(before: &Table, after: &Table) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_tables(&[], before, after, &mut ops);
    ops
}

fn diff_tables(path: &[Seg], before: &Table, after: &Table, ops: &mut Vec<Op>) {
    for (key, new) in after {
        let here = extend(path, Seg::Key(key.clone()));
        match before.get(key) {
            None => ops.push(Op::Set(here, new.clone())),
            Some(old) => diff_values(&here, old, new, ops),
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            ops.push(Op::Remove(extend(path, Seg::Key(key.clone()))));
        }
    }
}

fn diff_values(path: &[Seg], old: &Value, new: &Value, ops: &mut Vec<Op>) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Table(a), Value::Table(b)) => diff_tables(path, a, b, ops),
        (Value::Array(a), Value::Array(b)) => diff_arrays(path, a, b, ops, new),
        _ => ops.push(Op::Set(path.to_vec(), new.clone())),
    }
}

fn diff_arrays(path: &[Seg], old: &[Value], new: &[Value], ops: &mut Vec<Op>, whole: &Value) {
    let key = identity_key(new)
        .filter(|k| old.is_empty() || identity_key(old) == Some(*k))
        .or_else(|| {
            if new.is_empty() {
                identity_key(old)
            } else {
                None
            }
        });
    let Some(key) = key else {
        ops.push(Op::Set(path.to_vec(), whole.clone()));
        return;
    };
    let ids = |items: &[Value]| -> Vec<String> {
        items.iter().filter_map(|v| identity_of(v, key)).collect()
    };
    let (old_ids, new_ids) = (ids(old), ids(new));
    let common_old: Vec<&String> = old_ids.iter().filter(|id| new_ids.contains(id)).collect();
    let common_new: Vec<&String> = new_ids.iter().filter(|id| old_ids.contains(id)).collect();
    if common_old != common_new {
        // Порядок общих элементов изменился: точечно его не выразить.
        ops.push(Op::Set(path.to_vec(), whole.clone()));
        return;
    }
    for item in new {
        let Some(id) = identity_of(item, key) else {
            continue;
        };
        match old
            .iter()
            .find(|o| identity_of(o, key).as_deref() == Some(&id))
        {
            Some(previous) => {
                if let (Value::Table(a), Value::Table(b)) = (previous, item) {
                    diff_tables(&extend(path, Seg::Elem(id)), a, b, ops);
                }
            }
            None => ops.push(Op::Push(path.to_vec(), item.clone())),
        }
    }
    for id in old_ids {
        if !new_ids.contains(&id) {
            ops.push(Op::RemoveElem(path.to_vec(), id));
        }
    }
}

// ── применение к toml_edit ──────────────────────────────────────────────

/// Значение `toml` как элемент `toml_edit`. Кружным путём через текст: так
/// форматирование выбирает сам писатель TOML.
fn to_item(value: &Value) -> Option<Item> {
    let mut wrapper = Table::new();
    wrapper.insert("v".to_string(), value.clone());
    let text = toml::to_string(&wrapper).ok()?;
    let doc: DocumentMut = text.parse().ok()?;
    let mut item = doc.get("v")?.clone();
    clear_positions(&mut item);
    Some(item)
}

fn clear_positions(item: &mut Item) {
    match item {
        Item::Table(table) => {
            table.set_position(None);
            for (_, child) in table.iter_mut() {
                clear_positions(child);
            }
        }
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                table.set_position(None);
                for (_, child) in table.iter_mut() {
                    clear_positions(child);
                }
            }
        }
        _ => {}
    }
}

fn tables_mut(item: &mut Item) -> Vec<&mut dyn TableLike> {
    match item {
        Item::ArrayOfTables(array) => array
            .iter_mut()
            .map(|table| table as &mut dyn TableLike)
            .collect(),
        Item::Value(toml_edit::Value::Array(array)) => array
            .iter_mut()
            .filter_map(|value| value.as_inline_table_mut().map(|t| t as &mut dyn TableLike))
            .collect(),
        _ => Vec::new(),
    }
}

fn table_identity(table: &dyn TableLike, id: &str) -> bool {
    IDENTITY_KEYS
        .iter()
        .any(|key| table.get(key).and_then(Item::as_str) == Some(id))
}

fn descend<'a>(
    table: &'a mut dyn TableLike,
    path: &[Seg],
    create: bool,
) -> Option<&'a mut dyn TableLike> {
    let Some((first, rest)) = path.split_first() else {
        return Some(table);
    };
    let Seg::Key(key) = first else {
        return None;
    };
    if let Some(Seg::Elem(id)) = rest.first() {
        let item = table.get_mut(key)?;
        let found = tables_mut(item)
            .into_iter()
            .find(|candidate| table_identity(&**candidate, id))?;
        return descend(found, &rest[1..], create);
    }
    if create && table.get(key).is_none() {
        let mut fresh = toml_edit::Table::new();
        fresh.set_implicit(true);
        table.insert(key, Item::Table(fresh));
    }
    descend(table.get_mut(key)?.as_table_like_mut()?, rest, create)
}

/// Положить элемент, сохранив комментарии вокруг прежнего значения.
fn put(table: &mut dyn TableLike, key: &str, new: Item) {
    match (table.get_mut(key), new) {
        (Some(Item::Value(old)), Item::Value(mut fresh)) => {
            let decor = old.decor().clone();
            *fresh.decor_mut() = decor;
            *old = fresh;
        }
        (Some(slot), fresh) => *slot = fresh,
        (None, fresh) => {
            table.insert(key, fresh);
        }
    }
}

/// Применяет разницу. Возвращает пометки о том, что применить не удалось.
pub fn apply(doc: &mut DocumentMut, ops: &[Op]) -> Vec<String> {
    let mut skipped = Vec::new();
    for op in ops {
        let ok = match op {
            Op::Set(path, value) => apply_set(doc, path, value),
            Op::Remove(path) => apply_remove(doc, path),
            Op::Push(path, value) => apply_push(doc, path, value),
            Op::RemoveElem(path, id) => apply_remove_elem(doc, path, id),
        };
        if !ok {
            skipped.push(describe_op(op));
        }
    }
    skipped
}

fn describe_op(op: &Op) -> String {
    let (Op::Set(path, _) | Op::Remove(path) | Op::Push(path, _) | Op::RemoveElem(path, _)) = op;
    path.iter()
        .map(|seg| match seg {
            Seg::Key(k) => k.clone(),
            Seg::Elem(id) => format!("[{id}]"),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn split_last(path: &[Seg]) -> Option<(&[Seg], &str)> {
    match path.split_last()? {
        (Seg::Key(key), parent) => Some((parent, key)),
        _ => None,
    }
}

fn apply_set(doc: &mut DocumentMut, path: &[Seg], value: &Value) -> bool {
    let Some((parent, key)) = split_last(path) else {
        return false;
    };
    let Some(item) = to_item(value) else {
        return false;
    };
    let Some(table) = descend(doc.as_table_mut(), parent, true) else {
        return false;
    };
    put(table, key, item);
    true
}

fn apply_remove(doc: &mut DocumentMut, path: &[Seg]) -> bool {
    let Some((parent, key)) = split_last(path) else {
        return false;
    };
    let Some(table) = descend(doc.as_table_mut(), parent, false) else {
        // Родителя нет в файле (значение пришло из другого слоя): удалять нечего.
        return false;
    };
    table.remove(key);
    true
}

fn apply_push(doc: &mut DocumentMut, path: &[Seg], value: &Value) -> bool {
    let Some((parent, key)) = split_last(path) else {
        return false;
    };
    let Some(Item::Table(table)) = to_item(value) else {
        return false;
    };
    let Some(owner) = descend(doc.as_table_mut(), parent, true) else {
        return false;
    };
    let id = IDENTITY_KEYS
        .iter()
        .find_map(|k| table.get(k).and_then(Item::as_str).map(str::to_string));
    match owner.get_mut(key) {
        Some(Item::ArrayOfTables(array)) => {
            // Повторное применение той же правки не должно плодить копии.
            let known = id
                .as_deref()
                .and_then(|id| array.iter().position(|t| table_identity(t, id)));
            match known {
                Some(index) => {
                    array.remove(index);
                    array.push(table);
                }
                None => array.push(table),
            }
        }
        Some(Item::Value(toml_edit::Value::Array(array))) => {
            array.push(toml_edit::Value::InlineTable(table.into_inline_table()));
        }
        Some(slot) => {
            let mut array = toml_edit::ArrayOfTables::new();
            array.push(table);
            *slot = Item::ArrayOfTables(array);
        }
        None => {
            let mut array = toml_edit::ArrayOfTables::new();
            array.push(table);
            owner.insert(key, Item::ArrayOfTables(array));
        }
    }
    true
}

fn apply_remove_elem(doc: &mut DocumentMut, path: &[Seg], id: &str) -> bool {
    let Some((parent, key)) = split_last(path) else {
        return false;
    };
    let Some(owner) = descend(doc.as_table_mut(), parent, false) else {
        return false;
    };
    match owner.get_mut(key) {
        Some(Item::ArrayOfTables(array)) => {
            // Уже нет в файле: повторное применение правки не ошибка.
            let found = array.iter().position(|t| table_identity(t, id));
            if let Some(index) = found {
                array.remove(index);
            }
            true
        }
        Some(Item::Value(toml_edit::Value::Array(array))) => {
            let Some(index) = array
                .iter()
                .position(|v| v.as_inline_table().is_some_and(|t| table_identity(t, id)))
            else {
                return false;
            };
            array.remove(index);
            true
        }
        _ => false,
    }
}

// ── сохранение ──────────────────────────────────────────────────────────

/// Что вышло из сохранения.
#[derive(Debug, Default)]
pub struct SaveReport {
    pub wrote: bool,
    /// Правки, которых в этом файле нет по смыслу (значение из проектного слоя).
    pub skipped: Vec<String>,
}

fn reference_set(doc: &mut DocumentMut) -> Vec<String> {
    super::fields::reference_slots(doc)
        .into_iter()
        .map(|(_, reference)| reference)
        .collect()
}

#[derive(Debug)]
pub enum WriteError {
    /// Файл существует и не читается как TOML или как конфиг: не трогаем.
    Damaged(String),
    /// Запись убрала бы ссылку на секрет, который не удалось прочитать: не пишем.
    Refused(String),
    Io(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Damaged(message) => write!(
                f,
                "{message}. The file was left untouched: fix or move it, then try again"
            ),
            Self::Refused(message) | Self::Io(message) => f.write_str(message),
        }
    }
}

/// Читает текст файла; отсутствие — `None`.
fn read_existing(path: &Path) -> Result<Option<String>, WriteError> {
    match super::lock::retry_transient(|| crate::util::read_text(path)) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WriteError::Io(format!("{}: {}", path.display(), e.kind()))),
    }
}

pub fn parse_document(path: &Path, text: &str) -> Result<DocumentMut, WriteError> {
    text.parse::<DocumentMut>()
        .map_err(|e| WriteError::Damaged(describe_edit(path, text, &e)))
}

/// Результат должен остаться читаемым конфигом, иначе файл не пишем.
fn check_loadable(path: &Path, text: &str) -> Result<(), WriteError> {
    super::parse(text)
        .map(|_| ())
        .map_err(|e| WriteError::Damaged(describe(path, text, &e)))
}

/// Права владельца на файл с секретами. На Windows режимов нет: наследуется ACL профиля.
pub fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Записать текст атомарно и закрыть права.
pub fn write_file(path: &Path, text: &str) -> Result<(), WriteError> {
    super::lock::retry_transient(|| crate::util::atomic_write(path, text.as_bytes()))
        .map_err(|e| WriteError::Io(format!("{}: {}", path.display(), e.kind())))?;
    restrict_to_owner(path);
    Ok(())
}

/// Править файл под блокировкой: прочитать, дать правщику документ, проверить, записать.
/// Повреждённый файл возвращает ошибку и остаётся как был.
pub fn with_document<T>(
    path: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> Result<T, String>,
) -> Result<(T, bool), WriteError> {
    let _guard =
        FileLock::acquire(path).map_err(|e| WriteError::Io(format!("{}: {e}", path.display())))?;
    let existing = read_existing(path)?;
    let mut doc = match &existing {
        Some(text) => parse_document(path, text)?,
        None => DocumentMut::new(),
    };
    let out = edit(&mut doc).map_err(WriteError::Io)?;
    let text = doc.to_string();
    if existing.as_deref() == Some(text.as_str()) {
        return Ok((out, false));
    }
    check_loadable(path, &text)?;
    write_file(path, &text)?;
    Ok((out, true))
}

/// Сохранить разницу между `baseline` и `current` в файл.
pub fn save_diff(
    path: &Path,
    baseline: Option<&Table>,
    current: &Table,
    guard_refs: &[String],
) -> Result<SaveReport, WriteError> {
    let _guard =
        FileLock::acquire(path).map_err(|e| WriteError::Io(format!("{}: {e}", path.display())))?;
    let existing = read_existing(path)?;
    let mut doc = match &existing {
        Some(text) => parse_document(path, text)?,
        None => DocumentMut::new(),
    };
    let fallback;
    let before = match baseline {
        Some(table) => table,
        None => {
            // Загрузка не оставила снимка: за основу берём то, что лежит в файле.
            fallback = match &existing {
                Some(text) => super::parse(text)
                    .map_err(|e| WriteError::Damaged(describe(path, text, &e)))
                    .and_then(|config| {
                        Table::try_from(&config).map_err(|e| WriteError::Io(e.to_string()))
                    })?,
                None => Table::try_from(super::Config::default())
                    .map_err(|e| WriteError::Io(e.to_string()))?,
            };
            &fallback
        }
    };
    let ops = diff(before, current);
    let refs_before = reference_set(&mut doc);
    let skipped = apply(&mut doc, &ops);
    let kept_refs = reference_set(&mut doc);
    let removed_refs: Vec<String> = refs_before
        .into_iter()
        .filter(|r| !kept_refs.contains(r))
        .collect();
    // Ссылку, которую не смогли прочитать, не затираем: ключ, возможно, цел, а хранилище лишь
    // не ответило. Заменить её может только человек, поправив файл или хранилище.
    if let Some(blocked) = removed_refs.iter().find(|r| guard_refs.contains(r)) {
        return Err(WriteError::Refused(format!(
            "this change would drop {blocked}, which could not be read just now; fix the keyring (see `pooprusteek doctor`) and retry. Nothing was written"
        )));
    }
    let text = doc.to_string();
    let mut report = SaveReport {
        wrote: false,
        skipped,
    };
    if existing.as_deref() == Some(text.as_str()) || (existing.is_none() && ops.is_empty()) {
        return Ok(report);
    }
    check_loadable(path, &text)?;
    write_file(path, &text)?;
    report.wrote = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        toml::from_str(text).unwrap()
    }

    fn applied(base: &str, before: &str, after: &str) -> String {
        let mut doc: DocumentMut = base.parse().unwrap();
        let skipped = apply(&mut doc, &diff(&table(before), &table(after)));
        assert!(skipped.is_empty(), "{skipped:?}");
        doc.to_string()
    }

    #[test]
    fn a_changed_value_keeps_comments_and_neighbours() {
        let base = "# мой конфиг\n[ui]\ntheme = \"nord\" # любимая\nextra = 1\n\n[agent]\nmax_retries = 2\n";
        let out = applied(base, "[ui]\ntheme = \"nord\"\n", "[ui]\ntheme = \"dark\"\n");
        assert!(out.contains("# мой конфиг"), "{out}");
        assert!(out.contains("theme = \"dark\" # любимая"), "{out}");
        assert!(out.contains("extra = 1"), "unknown key must stay: {out}");
        assert!(out.contains("max_retries = 2"), "{out}");
    }

    #[test]
    fn nothing_changed_means_no_operations() {
        let both = "[ui]\ntheme = \"nord\"\n[[providers]]\nname = \"a\"\n";
        assert!(diff(&table(both), &table(both)).is_empty());
    }

    #[test]
    fn a_removed_key_leaves_only_that_key_out() {
        let base = "[provider]\ntoken = \"x\"\nmodel = \"m\"\n";
        let out = applied(
            base,
            "[provider]\ntoken = \"x\"\nmodel = \"m\"\n",
            "[provider]\nmodel = \"m\"\n",
        );
        assert!(!out.contains("token"), "{out}");
        assert!(out.contains("model = \"m\""), "{out}");
    }

    #[test]
    fn table_arrays_merge_by_name_not_position() {
        let base = "[[providers]]\nname = \"a\"\nmodel = \"1\"\n# про b\n[[providers]]\nname = \"b\"\nmodel = \"2\"\n";
        let before = "[[providers]]\nname = \"a\"\nmodel = \"1\"\n[[providers]]\nname = \"b\"\nmodel = \"2\"\n";
        let after = "[[providers]]\nname = \"a\"\nmodel = \"1\"\n[[providers]]\nname = \"b\"\nmodel = \"9\"\n[[providers]]\nname = \"c\"\nmodel = \"3\"\n";
        let out = applied(base, before, after);
        assert!(out.contains("# про b"), "{out}");
        assert!(out.contains("model = \"9\""), "{out}");
        assert!(out.contains("name = \"c\""), "{out}");
        assert!(out.contains("model = \"1\""), "{out}");
    }

    #[test]
    fn removing_an_element_removes_only_it() {
        let base = "[[providers]]\nname = \"a\"\n[[providers]]\nname = \"b\"\n";
        let out = applied(
            base,
            "[[providers]]\nname = \"a\"\n[[providers]]\nname = \"b\"\n",
            "[[providers]]\nname = \"a\"\n",
        );
        assert!(
            out.contains("name = \"a\"") && !out.contains("name = \"b\""),
            "{out}"
        );
    }

    #[test]
    fn a_new_section_is_appended_without_disturbing_existing_ones() {
        let base = "[ui]\ntheme = \"x\"\n";
        let out = applied(
            base,
            "[ui]\ntheme = \"x\"\n",
            "[ui]\ntheme = \"x\"\n[update]\nauto = true\n",
        );
        assert!(out.starts_with("[ui]"), "{out}");
        assert!(out.contains("[update]\nauto = true"), "{out}");
    }

    #[test]
    fn a_change_inside_an_element_missing_from_the_file_is_reported_not_invented() {
        // Аккаунт пришёл из проектного слоя: в этом файле его нет.
        let mut doc: DocumentMut = "[ui]\ntheme = \"x\"\n".parse().unwrap();
        let before = table("[[accounts]]\nid = \"p\"\nlabel = \"a\"\n");
        let after = table("[[accounts]]\nid = \"p\"\nlabel = \"b\"\n");
        let skipped = apply(&mut doc, &diff(&before, &after));
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(!doc.to_string().contains("accounts"));
    }

    #[test]
    fn reordering_elements_rewrites_the_array_whole() {
        let before = table("[[routes]]\nid = \"a\"\n[[routes]]\nid = \"b\"\n");
        let after = table("[[routes]]\nid = \"b\"\n[[routes]]\nid = \"a\"\n");
        let ops = diff(&before, &after);
        assert!(matches!(ops.as_slice(), [Op::Set(..)]), "{ops:?}");
    }

    #[test]
    fn arrays_of_plain_values_are_replaced_whole() {
        let out = applied(
            "[skills]\nenabled = [\"a\"] # список\n",
            "[skills]\nenabled = [\"a\"]\n",
            "[skills]\nenabled = [\"a\", \"b\"]\n",
        );
        assert!(out.contains("\"b\""), "{out}");
        assert!(out.contains("# список"), "{out}");
    }

    fn dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pooprusteek-write-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saving_changes_the_file_in_place_and_keeps_what_a_human_wrote() {
        let dir = dir("save");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# заметка\n[ui]\ntheme = \"nord\"\nmy_note = \"keep\"\n",
        )
        .unwrap();
        let before = table("[ui]\ntheme = \"nord\"\n");
        let after = table("[ui]\ntheme = \"dark\"\n");
        let report = save_diff(&path, Some(&before), &after, &[]).unwrap();
        assert!(report.wrote);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# заметка") && text.contains("my_note = \"keep\""),
            "{text}"
        );
        assert!(text.contains("theme = \"dark\""), "{text}");
        assert!(!lock_exists(&path), "the lock must be released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn lock_exists(path: &Path) -> bool {
        super::super::lock::lock_path(path).exists()
    }

    #[test]
    fn a_damaged_file_is_refused_and_left_byte_for_byte() {
        let dir = dir("damaged");
        let path = dir.join("config.toml");
        let broken = "[ui\ntheme = \"nord\"\ntoken = \"sk-canary-0000\"\n";
        std::fs::write(&path, broken).unwrap();
        let error = save_diff(&path, None, &table("[ui]\ntheme = \"x\"\n"), &[]).unwrap_err();
        assert!(matches!(error, WriteError::Damaged(_)), "{error}");
        assert!(!error.to_string().contains("canary"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_parses_as_toml_but_not_as_config_is_also_refused() {
        let dir = dir("typed");
        let path = dir.join("config.toml");
        let typo = "[agent]\nmax_retries = \"many\"\n";
        std::fs::write(&path, typo).unwrap();
        let error = save_diff(&path, None, &table("[ui]\ntheme = \"x\"\n"), &[]).unwrap_err();
        assert!(matches!(error, WriteError::Damaged(_)), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), typo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_created_with_only_what_differs_from_the_defaults() {
        let dir = dir("fresh");
        let path = dir.join("nested").join("config.toml");
        let defaults = Table::try_from(super::super::Config::default()).unwrap();
        let mut changed = super::super::Config::default();
        changed.ui.theme = "nord".into();
        let current = Table::try_from(&changed).unwrap();
        let report = save_diff(&path, Some(&defaults), &current, &[]).unwrap();
        assert!(report.wrote);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("theme = \"nord\""), "{text}");
        assert!(
            !text.contains("[agent]"),
            "defaults are not spelled out: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_writers_changing_different_keys_both_land() {
        let dir = dir("race");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\ntheme = \"x\"\n").unwrap();
        let threads: Vec<_> = (0..6)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let before = table("");
                    let after = table(&format!("[k{index}]\nv = {index}\n"));
                    save_diff(&path, Some(&before), &after, &[]).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for index in 0..6 {
            assert!(
                text.contains(&format!("[k{index}]")),
                "lost k{index}: {text}"
            );
        }
        assert!(text.contains("theme = \"x\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_document_refuses_a_damaged_file_without_calling_the_editor() {
        let dir = dir("doc");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[[[").unwrap();
        let mut called = false;
        let result = with_document(&path, |_| {
            called = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!called);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
