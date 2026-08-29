//! Что человек разрешил выполнять без вопроса.
//!
//! Раньше это было множество **имён инструментов**: одно «always allow» на
//! `bash` открывало произвольную команду навсегда, а на `edit` — запись в
//! любой путь, и заодно убирало единственный экран, где виден этот путь.
//! Теперь у правила есть область.
//!
//! Область **типизована**, и это не украшение. Первая попытка держала её
//! строкой и сравнивала одним сплиттером по `/`, `\` и пробелу сразу — из-за
//! чего правило на папку `src/app` покрывало соседнюю `src/app secrets`, а
//! правило на команду `cat etc/hosts` покрывало `cat /etc/hosts`. У команд и
//! у путей разные разделители, и смешивать их нельзя.

use crate::config::Config;
use crate::error::AppResult;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const WHITELIST_FILE: &str = "whitelist.json";

/// Область действия правила.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Начало команды словами: `["cargo", "test"]`.
    Command(Vec<String>),
    /// Папка, абсолютная и нормализованная. Только абсолютная: относительный
    /// `src` совпадал бы и с `<репозиторий>/src`, и с системным `/src`.
    Path(PathBuf),
}

impl Scope {
    /// Покрывает ли эта область запрошенную.
    fn covers(&self, asked: &Scope) -> bool {
        match (self, asked) {
            (Scope::Command(allowed), Scope::Command(asked)) => {
                !allowed.is_empty()
                    && asked.len() >= allowed.len()
                    && asked[..allowed.len()] == allowed[..]
            }
            (Scope::Path(allowed), Scope::Path(asked)) => path_covers(allowed, asked),
            // Разные виды области не сравнимы: команда никогда не покрывает
            // путь и наоборот.
            _ => false,
        }
    }

    /// Как область выглядит для человека.
    pub fn describe(&self) -> String {
        match self {
            Scope::Command(words) => words.join(" "),
            Scope::Path(path) => shorten_path(path),
        }
    }
}

/// Покрывает ли папка `allowed` путь `asked`. По компонентам, а не по строке;
/// на Windows регистр не важен — `src\App` и `src\app` одна папка.
fn path_covers(allowed: &Path, asked: &Path) -> bool {
    let parts = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let text = c.as_os_str().to_string_lossy().into_owned();
                if cfg!(windows) {
                    text.to_lowercase()
                } else {
                    text
                }
            })
            .collect()
    };
    let allowed = parts(allowed);
    let asked = parts(asked);
    !allowed.is_empty() && asked.len() >= allowed.len() && asked[..allowed.len()] == allowed[..]
}

/// Путь относительно рабочей папки, если он внутри неё. Абсолютные пути в
/// модалке не помещаются и прячут хвост за краем экрана.
fn shorten_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(relative) = path.strip_prefix(&cwd)
    {
        let shown = relative.to_string_lossy();
        return if shown.is_empty() {
            "the working directory".to_string()
        } else {
            format!("./{shown}")
        };
    }
    path.display().to_string()
}

/// Одно разрешение. `scope = None` — весь инструмент целиком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
}

impl Rule {
    pub fn tool(tool: &str) -> Self {
        Self {
            tool: tool.to_string(),
            scope: None,
        }
    }

    pub fn scoped(tool: &str, scope: Scope) -> Self {
        Self {
            tool: tool.to_string(),
            scope: Some(scope),
        }
    }

    /// Строка для списка и для пикера.
    pub fn describe(&self) -> String {
        match &self.scope {
            Some(scope) => format!("{} · {}", self.tool, scope.describe()),
            None => format!("{} · anything", self.tool),
        }
    }

    fn allows(&self, tool: &str, scope: Option<&Scope>) -> bool {
        if self.tool != tool {
            return false;
        }
        match (&self.scope, scope) {
            // Правило без области — весь инструмент.
            (None, _) => true,
            // Правило с областью против вызова, у которого её нет: разрешать
            // нечего, иначе сузить разрешение было бы невозможно.
            (Some(_), None) => false,
            (Some(allowed), Some(asked)) => allowed.covers(asked),
        }
    }
}

/// Набор правил. Порядок не важен — совпадение любого разрешает.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Whitelist {
    rules: Vec<Rule>,
}

impl Whitelist {
    pub fn from_rules(rules: Vec<Rule>) -> Self {
        let mut out = Self::default();
        for rule in rules {
            out.add(rule);
        }
        out
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn allows(&self, tool: &str, scope: Option<&Scope>) -> bool {
        self.rules.iter().any(|rule| rule.allows(tool, scope))
    }

    /// Какое правило разрешило вызов — для строки состояния: с областями
    /// «auto-approved» без объяснения перестал быть понятным.
    pub fn matching(&self, tool: &str, scope: Option<&Scope>) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.allows(tool, scope))
    }

    /// Добавить правило. Более широкое поглощает более узкие: держать и
    /// `bash · anything`, и `bash · cargo test` бессмысленно и путает в списке.
    pub fn add(&mut self, rule: Rule) {
        if self.allows(&rule.tool, rule.scope.as_ref()) {
            return;
        }
        self.rules.retain(|existing| {
            !(existing.tool == rule.tool
                && match (&rule.scope, &existing.scope) {
                    (None, _) => true,
                    (Some(wide), Some(narrow)) => wide.covers(narrow),
                    (Some(_), None) => false,
                })
        });
        self.rules.push(rule);
    }
}

fn whitelist_path() -> PathBuf {
    Config::data_dir().join(WHITELIST_FILE)
}

/// Файл читается в двух видах. Старый — массив имён (`["bash"]`); он значит
/// «весь инструмент» и переносится как есть, чтобы разрешение, которое человек
/// уже дал, не пропало молча при обновлении.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredWhitelist {
    Rules(Vec<Rule>),
    Legacy(Vec<String>),
}

/// Что дал разбор файла: сам список и что стоит сказать человеку.
pub struct Loaded {
    pub whitelist: Whitelist,
    pub notice: Option<String>,
}

/// Чистое ядро разбора — вся файловая система осталась в `load`.
pub fn parse(content: &str) -> Loaded {
    match serde_json::from_str::<StoredWhitelist>(content) {
        Ok(StoredWhitelist::Rules(rules)) => Loaded {
            whitelist: Whitelist::from_rules(rules),
            notice: None,
        },
        Ok(StoredWhitelist::Legacy(names)) => {
            let count = names.len();
            Loaded {
                whitelist: Whitelist::from_rules(names.iter().map(|n| Rule::tool(n)).collect()),
                // Старое разрешение человек давал в мире, где узкого варианта
                // не было: он выбирал «не спрашивать», а не «широко».
                notice: (count > 0).then(|| {
                    format!(
                        "{count} saved approval(s) imported as tool-wide — review them with /whitelist"
                    )
                }),
            }
        }
        Err(error) => Loaded {
            whitelist: Whitelist::default(),
            notice: Some(format!(
                "whitelist.json could not be read ({error}) — no tools are auto-approved; the file was kept as whitelist.json.bad"
            )),
        },
    }
}

pub fn load() -> Loaded {
    let path = whitelist_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Loaded {
            whitelist: Whitelist::default(),
            notice: None,
        };
    };
    let loaded = parse(&content);
    // Битый файл не затираем следующей записью молча: разрешения человека
    // должны остаться хотя бы в виде файла, который можно посмотреть.
    if loaded.notice.is_some() && loaded.whitelist.rules().is_empty() {
        let _ = std::fs::rename(&path, path.with_extension("json.bad"));
    }
    loaded
}

pub fn save(list: &Whitelist) -> AppResult<()> {
    let path = whitelist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(list.rules())?;
    crate::util::atomic_write(&path, content.as_bytes())?;
    Ok(())
}

/// Добавить одно правило в сохранённый список (чтение-правка-запись). Зовётся
/// из модалки, чтобы выбор пережил и `/whitelist`, и перезапуск. Возвращает
/// ошибку записи: молча разойтись с диском хуже, чем сказать.
pub fn persist(rule: Rule) -> Result<(), String> {
    let mut list = load().whitelist;
    let shown = rule.describe();
    list.add(rule);
    save(&list).map_err(|e| format!("Failed to save the approval for {shown}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(words: &[&str]) -> Scope {
        Scope::Command(words.iter().map(|w| w.to_string()).collect())
    }

    fn dir(path: &str) -> Scope {
        Scope::Path(PathBuf::from(path))
    }

    #[test]
    fn a_tool_wide_rule_allows_any_scope() {
        let list = Whitelist::from_rules(vec![Rule::tool("bash")]);
        assert!(list.allows("bash", Some(&cmd(&["rm", "-rf"]))));
        assert!(list.allows("bash", None));
        assert!(!list.allows("powershell", Some(&cmd(&["ls"]))));
    }

    #[test]
    fn a_command_rule_allows_only_inside_its_scope() {
        let list = Whitelist::from_rules(vec![Rule::scoped("bash", cmd(&["cargo", "test"]))]);
        assert!(list.allows("bash", Some(&cmd(&["cargo", "test"]))));
        assert!(list.allows("bash", Some(&cmd(&["cargo", "test", "--release"]))));
        assert!(!list.allows("bash", Some(&cmd(&["cargo", "publish"]))));
    }

    #[test]
    fn a_command_word_is_never_a_string_prefix() {
        // Иначе `cargo test-and-delete` прошло бы по правилу `cargo test`.
        let list = Whitelist::from_rules(vec![Rule::scoped("bash", cmd(&["cargo", "test"]))]);
        assert!(!list.allows("bash", Some(&cmd(&["cargo", "test-and-delete"]))));
    }

    #[test]
    fn a_path_rule_is_compared_by_segments() {
        let list = Whitelist::from_rules(vec![Rule::scoped("edit", dir("/repo/src/app"))]);
        assert!(list.allows("edit", Some(&dir("/repo/src/app"))));
        assert!(list.allows("edit", Some(&dir("/repo/src/app/keys"))));
        assert!(!list.allows("edit", Some(&dir("/repo/src/application-secrets"))));
        assert!(!list.allows("edit", Some(&dir("/repo/src/tools"))));
    }

    #[test]
    fn a_directory_with_a_space_is_not_confused_with_a_nested_one() {
        // Один сплиттер по `/` и пробелу давал ложное разрешение в обе
        // стороны — ради этого область и типизована.
        let list = Whitelist::from_rules(vec![Rule::scoped("edit", dir("/repo/src/app"))]);
        assert!(!list.allows("edit", Some(&dir("/repo/src/app secrets"))));

        let spaced = Whitelist::from_rules(vec![Rule::scoped("edit", dir("/repo/my app"))]);
        assert!(!spaced.allows("edit", Some(&dir("/repo/my/app/deep"))));
    }

    #[test]
    fn a_command_scope_never_matches_a_path_scope() {
        let list = Whitelist::from_rules(vec![Rule::scoped("bash", cmd(&["cat", "etc"]))]);
        assert!(!list.allows("bash", Some(&dir("/etc"))));
    }

    #[test]
    fn a_scoped_rule_does_not_cover_a_call_without_a_scope() {
        // Иначе сузить разрешение было бы невозможно.
        let list = Whitelist::from_rules(vec![Rule::scoped("bash", cmd(&["cargo"]))]);
        assert!(!list.allows("bash", None));
    }

    #[test]
    fn a_wider_rule_replaces_the_narrow_ones_it_covers() {
        let mut list = Whitelist::from_rules(vec![
            Rule::scoped("bash", cmd(&["cargo", "test"])),
            Rule::scoped("bash", cmd(&["cargo", "build"])),
        ]);
        list.add(Rule::scoped("bash", cmd(&["cargo"])));
        assert_eq!(list.rules().len(), 1);
        assert_eq!(list.rules()[0], Rule::scoped("bash", cmd(&["cargo"])));
    }

    #[test]
    fn a_tool_wide_rule_replaces_every_scoped_one() {
        let mut list = Whitelist::from_rules(vec![
            Rule::scoped("bash", cmd(&["cargo", "test"])),
            Rule::scoped("bash", cmd(&["git", "status"])),
        ]);
        list.add(Rule::tool("bash"));
        assert_eq!(list.rules(), &[Rule::tool("bash")]);
    }

    #[test]
    fn a_narrow_rule_is_dropped_when_a_wider_one_already_allows_it() {
        let mut list = Whitelist::from_rules(vec![Rule::tool("bash")]);
        list.add(Rule::scoped("bash", cmd(&["cargo", "test"])));
        assert_eq!(list.rules(), &[Rule::tool("bash")]);
    }

    #[test]
    fn two_unrelated_scopes_are_both_kept() {
        // Первая версия теряла правило: `src app` и `src/app` считались одним
        // из-за общего сплиттера, и второе молча не сохранялось.
        let mut list = Whitelist::from_rules(vec![Rule::scoped("edit", dir("/repo/src app"))]);
        list.add(Rule::scoped("edit", dir("/repo/src/app")));
        assert_eq!(list.rules().len(), 2, "{:?}", list.rules());
    }

    #[test]
    fn rules_of_different_tools_do_not_interfere() {
        let mut list = Whitelist::from_rules(vec![Rule::scoped("bash", cmd(&["cargo"]))]);
        list.add(Rule::scoped("edit", dir("/repo/src")));
        assert_eq!(list.rules().len(), 2);
    }

    #[test]
    fn matching_names_the_rule_that_allowed_the_call() {
        let list = Whitelist::from_rules(vec![Rule::scoped("bash", cmd(&["cargo", "test"]))]);
        let rule = list.allows("bash", Some(&cmd(&["cargo", "test", "-q"])));
        assert!(rule);
        let named = list
            .matching("bash", Some(&cmd(&["cargo", "test", "-q"])))
            .unwrap();
        assert!(named.describe().contains("cargo test"));
    }

    #[test]
    fn the_old_file_format_is_imported_and_announced() {
        // Разрешение, которое человек уже дал, не пропадает — но и не
        // переносится молча: широким оно стало не по его выбору.
        let loaded = parse(r#"["bash","edit"]"#);
        assert!(loaded.whitelist.allows("bash", Some(&cmd(&["anything"]))));
        assert!(loaded.notice.unwrap().contains("2 saved approval"));
    }

    #[test]
    fn an_empty_list_is_not_announced_as_a_migration() {
        assert!(parse("[]").notice.is_none());
    }

    #[test]
    fn a_broken_file_reports_it_instead_of_pretending_to_be_empty() {
        let loaded = parse("{not json");
        assert!(loaded.whitelist.rules().is_empty());
        assert!(loaded.notice.unwrap().contains("could not be read"));
    }

    #[test]
    fn the_new_file_format_round_trips() {
        let list = Whitelist::from_rules(vec![
            Rule::tool("read_file"),
            Rule::scoped("bash", cmd(&["cargo", "test"])),
            Rule::scoped("edit", dir("/repo/src")),
        ]);
        let text = serde_json::to_string(list.rules()).unwrap();
        assert_eq!(parse(&text).whitelist, list);
    }

    #[test]
    fn a_path_scope_ignores_case_on_windows_only() {
        let list = Whitelist::from_rules(vec![Rule::scoped("edit", dir("/repo/src/App"))]);
        let asked = dir("/repo/src/app");
        assert_eq!(list.allows("edit", Some(&asked)), cfg!(windows));
    }
}
