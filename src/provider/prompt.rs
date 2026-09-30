//! DeepSeek prompt assembly.
//!
//! DeepSeek's web API takes a single flat `prompt` string rather than a
//! role-tagged message array, and treats the first turn of a session
//! differently from later ones (the system prompt and local history are only
//! sent once). This module owns that translation from [`ChatMessage`]s to the
//! wire prompt. It is pure — no `self`, no I/O — so the bug-prone assembly
//! rules live in one place and are covered by tests.

use super::{ChatMessage, Role};
use regex::Regex;
use std::sync::LazyLock;

/// Code fences of 300+ chars are collapsed to `[...]` when replaying assistant
/// history, so old code dumps don't blow up the prompt budget.
static LONG_CODE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.{300,}?```").expect("hardcoded regex is valid"));

/// Pull the first `System` message out as the system prompt; return the rest in
/// order. Later system messages (if any) stay in the non-system list.
pub(crate) fn split_system_prompt(messages: &[ChatMessage]) -> (String, Vec<ChatMessage>) {
    let mut system_prompt = String::new();
    let mut non_system = Vec::new();
    let mut captured_system = false;

    for message in messages {
        if !captured_system && message.role == Role::System {
            system_prompt = message.content.clone();
            captured_system = true;
        } else {
            non_system.push(message.clone());
        }
    }

    (system_prompt, non_system)
}

fn strip_long_code_blocks(text: &str) -> String {
    LONG_CODE_BLOCK_RE.replace_all(text, "[...]").into_owned()
}

fn format_history_message(message: &ChatMessage) -> String {
    if message.role == Role::Assistant {
        let stripped = strip_long_code_blocks(message.content.trim());
        return format!("[ASSISTANT]\n{stripped}");
    }

    let role = match message.role {
        Role::System => "SYSTEM",
        Role::User => "USER",
        Role::Assistant => "ASSISTANT",
        Role::Tool => "TOOL",
    };
    format!(
        "[{role}]\n{}{}",
        message.content,
        lost_attachments_note(message)
    )
}

/// Вложения из пересылаемой истории заново не грузятся ([`tail`]), и модель
/// должна знать, что их содержимого в этой сессии нет — есть только пути.
fn lost_attachments_note(message: &ChatMessage) -> String {
    if message.attachments.is_empty() {
        return String::new();
    }
    format!(
        "\n(Содержимое вложений этого сообщения в новую сессию не перенесено: {}. Если оно нужно, попроси приложить файлы снова.)",
        message.attachments.join(", ")
    )
}

/// Trailing one-line format anchor appended to every non-empty send. The
/// system prompt is delivered once per session and then only lives far back
/// in DeepSeek's server-side context; a weak model holds the tool-call
/// format by recency far better than by primacy, and this costs ~60 tokens
/// per turn.
const FORMAT_REMINDER: &str = "[Напоминание формата: инструменты вызывай только блоком <tool_use><name>…</name><arguments>{JSON}</arguments></tool_use>, после </tool_use> — стоп. Другую разметку вызовов (DSML, invoke, function_calls) не используй. Имена инструментов не изобретай, результаты не выдумывай — жди TOOL RESULT.]";

const FILE_NAME_PREFIX: &str = "[file name]: ";

/// Текст в рамке, которой веб-чат DeepSeek подаёт модели прикреплённый файл
/// (снято с живого клиента). По токенам то же, что вложение, но без загрузки.
pub fn attached_file(name: &str, content: &str) -> String {
    format!(
        "{FILE_NAME_PREFIX}{name}\n[file content begin]\n{}\n[file content end]",
        content.trim()
    )
}

/// Имена файлов, приложенных к промпту рамкой [`attached_file`].
fn attached_file_names(prompt: &str) -> std::collections::BTreeSet<&str> {
    prompt
        .lines()
        .filter_map(|line| line.strip_prefix(FILE_NAME_PREFIX))
        .map(str::trim)
        .collect()
}

/// Приложенные файлы новой версии промпта против той, что держит сервер.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileChanges {
    added: Vec<String>,
    removed: Vec<String>,
    current: Vec<String>,
    /// Прежний набор известен. У подхваченной сессии нет: тогда отменить
    /// выключенное можно только через полный список действующих.
    known: bool,
}

impl FileChanges {
    pub(crate) fn between(held: Option<&str>, current: &str) -> Self {
        fn owned(names: impl Iterator<Item = impl ToString>) -> Vec<String> {
            names.map(|name| name.to_string()).collect()
        }
        let after = attached_file_names(current);
        let Some(held) = held else {
            return Self {
                added: Vec::new(),
                removed: Vec::new(),
                current: owned(after.iter()),
                known: false,
            };
        };
        let before = attached_file_names(held);
        Self {
            added: owned(after.difference(&before)),
            removed: owned(before.difference(&after)),
            current: owned(after.iter()),
            known: true,
        }
    }

    /// Строки шапки о файлах. Живой прогон 2026-09-30: без имён модель
    /// продолжала следовать выключенному скиллу, раз уже следовала ему выше.
    fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.removed.is_empty() {
            lines.push(format!(
                "Выключены файлы: {}. Их правила больше не действуют.",
                self.removed.join(", ")
            ));
        }
        if !self.added.is_empty() {
            lines.push(format!(
                "Подключены файлы: {}. Следуй им с этого сообщения.",
                self.added.join(", ")
            ));
        }
        let nothing_ever_attached =
            self.known && self.current.is_empty() && self.removed.is_empty();
        if nothing_ever_attached {
            return lines;
        }
        lines.push(if self.current.is_empty() {
            "Приложенных файлов сейчас нет: правила всех приложенных раньше файлов больше не действуют, даже если ты следовал им выше в этой беседе.".to_string()
        } else {
            format!(
                "Сейчас действуют только приложенные файлы: {}. Правила остальных приложенных раньше файлов больше не действуют, даже если ты следовал им выше в этой беседе.",
                self.current.join(", ")
            )
        });
        lines
    }
}

/// Что серверная сессия уже знает о системном промпте этой отправки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemDelivery {
    /// Новая сессия: промпт и локальная история уходят целиком.
    Fresh,
    /// Сервер держит этот же промпт: уходит только хвост.
    Held,
    /// Сервер держит устаревший промпт (включили скилл, подключился MCP).
    Changed(FileChanges),
}

/// Шапка новой версии промпта в уже идущей сессии: старая остаётся в
/// серверном контексте, и модели надо прямо сказать, какая из двух действует.
const SYSTEM_UPDATE_HEADER: &str = "### SYSTEM PROMPT UPDATE\nСистемные инструкции этой беседы изменились. Текст ниже целиком заменяет прежние системные инструкции: прежние больше не действуют.";

fn update_header(changes: &FileChanges) -> String {
    std::iter::once(SYSTEM_UPDATE_HEADER.to_string())
        .chain(changes.describe())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Новый ввод — всё после последнего ответа модели. Вложения едут только с ним:
/// старые файлы при пересылке истории не грузятся заново (пропавший ронял бы ход).
pub(crate) fn tail(messages: &[ChatMessage]) -> &[ChatMessage] {
    &messages[tail_start(messages)..]
}

/// Index where the "new input" tail begins: everything after the last
/// assistant message. That tail is what a continuing session actually needs
/// to send — typically one user message, a batch of tool results, or an
/// advisory system note (semantic hint) followed by the user message.
fn tail_start(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Render one tail message as a labeled section. `None` for empty user/system
/// content (nothing to say) and for assistant messages (excluded from the
/// tail by construction). Tool results keep an explicit section even when
/// empty so the model sees the call completed.
fn format_tail_message(message: &ChatMessage) -> Option<String> {
    match message.role {
        Role::Tool => Some(format!(
            "### TOOL RESULT: {}\n{}",
            message.name.as_deref().unwrap_or("unknown"),
            message.content
        )),
        Role::User if !message.content.is_empty() => {
            Some(format!("### USER INPUT\n{}", message.content))
        }
        Role::System if !message.content.is_empty() => {
            Some(format!("### NOTE\n{}", message.content))
        }
        _ => None,
    }
}

fn render_tail(tail: &[ChatMessage]) -> String {
    tail.iter()
        .filter_map(format_tail_message)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// True when the local transcript holds exactly one real conversational
/// message (the newest user input) — i.e. this is the first send of a brand
/// new conversation on a possibly-reused provider session. Advisory system
/// notes (semantic hints) are not conversational and don't count.
pub(crate) fn is_first_conversational_send(non_system_messages: &[ChatMessage]) -> bool {
    non_system_messages
        .iter()
        .filter(|m| m.role != Role::System)
        .count()
        == 1
}

/// Build the flat prompt string sent to DeepSeek.
///
/// On the first send of a session ([`SystemDelivery::Fresh`]) the system
/// prompt and prior turns are embedded as local memory. On later sends only
/// the tail after the last assistant message goes out (the newest user input,
/// tool-result batch, and any system note), since DeepSeek retains the rest
/// server-side — preceded by the whole new system prompt when it changed
/// since the server got it. A user message in the tail always renders as its
/// own `### USER INPUT` section — system notes must never displace it.
pub(crate) fn build_prompt(
    messages: &[ChatMessage],
    system_prompt: &str,
    delivery: SystemDelivery,
) -> String {
    if messages.is_empty() {
        return system_prompt.trim().to_string();
    }

    let tail_from = tail_start(messages);
    let tail = render_tail(&messages[tail_from..]);

    match delivery {
        SystemDelivery::Held if tail.is_empty() => return tail,
        SystemDelivery::Held => return format!("{tail}\n\n{FORMAT_REMINDER}"),
        // Новая версия уходит и при пустом хвосте: сессия засчитает её
        // доставленной, значит она обязана быть в отправленном тексте.
        SystemDelivery::Changed(changes) => {
            let update = format!("{}\n\n{}", update_header(&changes), system_prompt.trim());
            if tail.is_empty() {
                return update;
            }
            return format!("{update}\n\n{tail}\n\n{FORMAT_REMINDER}");
        }
        SystemDelivery::Fresh => {}
    }

    let mut parts = Vec::new();

    if !system_prompt.trim().is_empty() {
        parts.push(system_prompt.trim().to_string());
    }

    if tail_from > 0 {
        let history = messages[..tail_from]
            .iter()
            .map(format_history_message)
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(String::new());
        parts.push("### LOCAL MEMORY".to_string());
        parts.push(history);
    }

    if !tail.is_empty() {
        parts.push(String::new());
        parts.push(tail);
        parts.push(String::new());
        parts.push(FORMAT_REMINDER.to_string());
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_captures_first_system_only() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::system("late"),
        ];
        let (system, rest) = split_system_prompt(&messages);
        assert_eq!(system, "sys");
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].role, Role::User);
        assert_eq!(rest[1].role, Role::System); // a later system stays in the list
    }

    #[test]
    fn first_turn_embeds_system_and_user_input() {
        let messages = vec![ChatMessage::user("hello")];
        let prompt = build_prompt(&messages, "SYSTEM PROMPT", SystemDelivery::Fresh);
        assert!(prompt.starts_with("SYSTEM PROMPT"));
        assert!(prompt.contains("### USER INPUT\nhello"));
        assert!(!prompt.contains("### LOCAL MEMORY")); // single message → no history
    }

    #[test]
    fn first_turn_with_history_includes_local_memory() {
        let messages = vec![
            ChatMessage::user("earlier"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("now"),
        ];
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Fresh);
        assert!(prompt.contains("### LOCAL MEMORY"));
        assert!(prompt.contains("[ASSISTANT]\nreply"));
        assert!(prompt.contains("### USER INPUT\nnow"));
    }

    #[test]
    fn later_turn_sends_only_latest_user_input() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user("new"),
        ];
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Held);
        assert_eq!(prompt, format!("### USER INPUT\nnew\n\n{FORMAT_REMINDER}"));
        assert!(!prompt.contains("old")); // server already has earlier turns
    }

    #[test]
    fn later_turn_batches_trailing_tool_results() {
        let messages = vec![
            ChatMessage::user("q"),
            ChatMessage::assistant("<tool_use>…</tool_use>"),
            ChatMessage::tool_with_display("id1", "bash", "out-a", "out-a", false),
            ChatMessage::tool_with_display("id2", "grep", "out-b", "out-b", false),
        ];
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Held);
        assert_eq!(
            prompt,
            format!(
                "### TOOL RESULT: bash\nout-a\n\n### TOOL RESULT: grep\nout-b\n\n{FORMAT_REMINDER}"
            )
        );
    }

    /// Regression: the semantic hint is inserted *before* the newest user
    /// message; the user text must go out as its own `### USER INPUT`
    /// section, after the hint's `### NOTE`.
    #[test]
    fn hint_before_user_keeps_user_input_last() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::system("[Hint] maybe use skill X"),
            ChatMessage::user("real question"),
        ];
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Held);
        assert_eq!(
            prompt,
            format!(
                "### NOTE\n[Hint] maybe use skill X\n\n### USER INPUT\nreal question\n\n{FORMAT_REMINDER}"
            )
        );
    }

    /// Regression for the original bug: a trailing system note after the
    /// user message used to be sent as the sole `### USER INPUT`, silently
    /// dropping the user's actual message.
    #[test]
    fn trailing_system_note_never_displaces_user_input() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user("real question"),
            ChatMessage::system("[Hint] maybe use skill X"),
        ];
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Held);
        assert!(prompt.contains("### USER INPUT\nreal question"));
        assert!(prompt.contains("### NOTE\n[Hint] maybe use skill X"));
    }

    #[test]
    fn first_send_renders_hint_as_note_not_memory() {
        let messages = vec![
            ChatMessage::system("[Hint] maybe use skill X"),
            ChatMessage::user("hello"),
        ];
        let prompt = build_prompt(&messages, "SYSTEM PROMPT", SystemDelivery::Fresh);
        assert!(prompt.starts_with("SYSTEM PROMPT"));
        assert!(prompt.contains("### NOTE\n[Hint] maybe use skill X"));
        assert!(prompt.contains("### USER INPUT\nhello"));
        assert!(!prompt.contains("### LOCAL MEMORY"));
    }

    #[test]
    fn empty_user_content_on_later_turn_sends_nothing() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user(""),
        ];
        assert_eq!(build_prompt(&messages, "SYS", SystemDelivery::Held), "");
    }

    /// Скилл включили посреди беседы: сервер держит старый промпт, и новый
    /// обязан уйти целиком — до ввода пользователя, чтобы тот остался последним.
    #[test]
    fn a_changed_system_prompt_is_resent_before_the_tail() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user("new"),
        ];
        let prompt = build_prompt(
            &messages,
            "NEW SYS",
            SystemDelivery::Changed(FileChanges::between(Some("SYS"), "NEW SYS")),
        );
        assert_eq!(
            prompt,
            format!(
                "{SYSTEM_UPDATE_HEADER}\n\nNEW SYS\n\n### USER INPUT\nnew\n\n{FORMAT_REMINDER}"
            )
        );
        assert!(!prompt.contains("old"), "history stays server-side");
    }

    /// Пустой хвост не повод потерять обновление: сессия засчитает его
    /// доставленным по факту отправки.
    #[test]
    fn a_changed_system_prompt_goes_out_even_with_an_empty_tail() {
        let messages = vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("a"),
            ChatMessage::user(""),
        ];
        assert_eq!(
            build_prompt(
                &messages,
                "NEW SYS",
                SystemDelivery::Changed(FileChanges::between(Some("SYS"), "NEW SYS"))
            ),
            format!("{SYSTEM_UPDATE_HEADER}\n\nNEW SYS")
        );
    }

    fn prompt_with(files: &[&str]) -> String {
        let blocks: Vec<String> = files
            .iter()
            .map(|name| attached_file(name, "body"))
            .collect();
        format!("BASE\n\n{}", blocks.join("\n"))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    /// Оба направления: включённый и выключенный файл названы, общий — нет.
    #[test]
    fn file_changes_name_what_was_added_and_removed() {
        let held = prompt_with(&["keep.md", "gone.md"]);
        let current = prompt_with(&["keep.md", "new.md"]);
        let changes = FileChanges::between(Some(&held), &current);
        assert_eq!(changes.added, names(&["new.md"]));
        assert_eq!(changes.removed, names(&["gone.md"]));

        let lines = changes.describe().join("\n");
        assert!(lines.contains("Выключены файлы: gone.md."), "{lines}");
        assert!(lines.contains("Подключены файлы: new.md."), "{lines}");
        assert!(lines.contains("действуют только приложенные файлы: keep.md, new.md"));
    }

    /// Промпт сменился не из-за файлов (подключился MCP) и файлов не было:
    /// шапка про файлы молчит.
    #[test]
    fn a_change_without_files_says_nothing_about_files() {
        assert!(FileChanges::between(Some("A"), "B").describe().is_empty());
    }

    /// Подхваченная сессия: прежний набор неизвестен, разницу не выдумываем, но
    /// полный список действующих отменяет всё остальное. Живой прогон
    /// 2026-09-30: без этого модель следовала выключенному скиллу.
    #[test]
    fn an_unknown_held_prompt_revokes_everything_not_listed() {
        let changes = FileChanges::between(None, &prompt_with(&["a.md"]));
        assert!(changes.added.is_empty() && changes.removed.is_empty());
        let lines = changes.describe().join("\n");
        assert!(
            lines.contains("действуют только приложенные файлы: a.md"),
            "{lines}"
        );

        let none = FileChanges::between(None, "BASE").describe().join("\n");
        assert!(none.contains("Приложенных файлов сейчас нет"), "{none}");
    }

    #[test]
    fn the_update_header_leads_the_resent_prompt() {
        let messages = vec![ChatMessage::assistant("a"), ChatMessage::user("go")];
        let changes = FileChanges::between(Some(&prompt_with(&["gone.md"])), "SYS");
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Changed(changes));
        assert!(prompt.starts_with(SYSTEM_UPDATE_HEADER), "{prompt}");
        assert!(prompt.contains("Выключены файлы: gone.md."), "{prompt}");
        assert!(prompt.ends_with(FORMAT_REMINDER));
    }

    #[test]
    fn first_conversational_send_ignores_system_notes() {
        assert!(is_first_conversational_send(&[ChatMessage::user("hi")]));
        assert!(is_first_conversational_send(&[
            ChatMessage::system("[Hint] x"),
            ChatMessage::user("hi"),
        ]));
        assert!(!is_first_conversational_send(&[
            ChatMessage::user("hi"),
            ChatMessage::assistant("a"),
            ChatMessage::user("more"),
        ]));
        assert!(!is_first_conversational_send(&[]));
    }

    #[test]
    fn empty_messages_returns_trimmed_system() {
        assert_eq!(build_prompt(&[], "  sys  ", SystemDelivery::Fresh), "sys");
    }

    #[test]
    fn long_code_blocks_collapse_in_history() {
        let big = format!("```\n{}\n```", "x".repeat(400));
        let messages = vec![ChatMessage::assistant(&big), ChatMessage::user("now")];
        let prompt = build_prompt(&messages, "", SystemDelivery::Fresh);
        assert!(prompt.contains("[...]"));
        assert!(!prompt.contains(&"x".repeat(400)));
    }

    /// Новая сессия пересылает историю текстом, а старые вложения заново не
    /// грузятся — модель узнаёт об этом из пометки у сообщения.
    #[test]
    fn replayed_history_says_old_attachments_did_not_travel() {
        let old = ChatMessage {
            attachments: vec!["C:/docs/scan.pdf".to_string()],
            ..ChatMessage::user("see the scan")
        };
        let messages = vec![old, ChatMessage::assistant("ok"), ChatMessage::user("now")];
        let prompt = build_prompt(&messages, "SYS", SystemDelivery::Fresh);
        assert!(
            prompt.contains("не перенесено: C:/docs/scan.pdf"),
            "{prompt}"
        );
        assert_eq!(tail(&messages).len(), 1, "only the new input carries files");
    }
}
