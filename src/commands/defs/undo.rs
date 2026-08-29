use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

/// `/undo` — откатить последнюю правку файла. Инструменты `edit`/`write`
/// снимают копию после записи; здесь она возвращается на место.
pub struct UndoCommand;

/// Сколько записей показываем в списке. Больше не помещается на экран, а
/// откатывают почти всегда последнюю.
const LIST_LIMIT: usize = 15;

impl Command for UndoCommand {
    fn name(&self) -> &str {
        "undo"
    }

    fn description(&self) -> &str {
        "Undo the agent's last file change"
    }

    fn usage(&self) -> &str {
        "/undo [list|skip]"
    }

    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        match args.trim() {
            "list" => {
                state.push_ui_system(&list());
                CommandResult::Handled
            }
            "skip" => match crate::checkpoints::Store::shared().skip_next() {
                Ok(report) => {
                    state.push_ui_system(&report);
                    CommandResult::Handled
                }
                Err(error) => CommandResult::Error(error),
            },
            // Перезапись файла пользователя и удаление созданного — тот же
            // класс, что `/wipe` и `/delete`, поэтому тот же экран.
            "" => match crate::checkpoints::Store::shared().next_undo() {
                Ok(entry) => CommandResult::ConfirmUndo {
                    target: entry.describe(),
                    destructive: matches!(entry.before, crate::checkpoints::Before::Absent),
                },
                Err(error) => CommandResult::Error(error),
            },
            other => CommandResult::Error(format!(
                "Unknown argument '{other}'. Usage: /undo [list|skip]"
            )),
        }
    }
}

fn list() -> String {
    let entries = crate::checkpoints::Store::shared().pending();
    if entries.is_empty() {
        return "No file changes left to undo.".to_string();
    }
    // `pending` уже отдаёт новые первыми: откатывают с конца, глаз ищет там же.
    let shown: Vec<String> = entries
        .iter()
        .take(LIST_LIMIT)
        .map(|entry| entry.describe())
        .collect();
    let mut out = format!("{} change(s) can be undone, newest first:", entries.len());
    out.push('\n');
    out.push_str(&shown.join("\n"));
    if entries.len() > LIST_LIMIT {
        out.push_str(&format!("\n… and {} older", entries.len() - LIST_LIMIT));
    }
    out
}
