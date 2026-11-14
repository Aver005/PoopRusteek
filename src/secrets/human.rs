//! Команды, меняющие доверие и секреты, выполняет только человек за терминалом.
//!
//! Это смягчение, а не абсолют: агент с `bash` работает от имени пользователя и может
//! править файлы напрямую. Но удобную дверь (`trust --yes`, `secrets bind` в канале) закрываем.

use std::io::IsTerminal;

/// Переменная, которую наш запуск инструментов ставит каждому ребёнку модели (канал и PTY).
pub const AGENT_CHILD_ENV: &str = "POOPRUSTEEK_AGENT_CHILD";

/// Ребёнок инструмента модели? Любое непустое значение считается «да».
pub fn is_agent_child() -> bool {
    std::env::var_os(AGENT_CHILD_ENV).is_some_and(|value| !value.is_empty())
}

/// Git Bash / mintty / MSYS на Windows: консоли у них нет, `IsTerminal` отвечает «нет» и человеку.
fn terminal_undetectable() -> bool {
    cfg!(windows) || std::env::var_os("MSYSTEM").is_some()
}

/// Чистое решение, чтобы его можно было проверить без терминала.
fn verdict(
    what: &str,
    stdin_tty: bool,
    stderr_tty: bool,
    agent_child: bool,
    undetectable: bool,
) -> Result<(), String> {
    if agent_child {
        return Err(format!(
            "{what} must be run by a person in their own terminal, not from an agent tool. \
             {AGENT_CHILD_ENV} is set: this terminal may have been started by an agent tool and \
             inherited it. If you are the person at the keyboard, open a terminal yourself, or clear \
             it (bash: `unset {AGENT_CHILD_ENV}`; PowerShell: `Remove-Item Env:{AGENT_CHILD_ENV}`; \
             cmd: `set {AGENT_CHILD_ENV}=`) and run the command again."
        ));
    }
    if !(stdin_tty && stderr_tty) {
        let mut message = format!(
            "{what} needs a real terminal and a person at it (stdin and stderr must be a TTY); \
             piped or scripted runs are refused."
        );
        if undetectable {
            message.push_str(
                " Git Bash (mintty) cannot be told apart from a pipe: run it through \
                 `winpty pooprusteek …`, or use PowerShell, cmd or Windows Terminal.",
            );
        }
        return Err(message);
    }
    Ok(())
}

/// Проверка для команд, которые нельзя отдавать скрипту: нужен настоящий TTY и не ребёнок агента.
pub fn require_human(what: &str) -> Result<(), String> {
    verdict(
        what,
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
        is_agent_child(),
        terminal_undetectable(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_at_a_terminal_passes() {
        assert!(verdict("trust", true, true, false, false).is_ok());
    }

    #[test]
    fn a_pipe_on_either_stream_is_refused() {
        let error = verdict("trust", false, true, false, false).unwrap_err();
        assert!(error.contains("real terminal"), "{error}");
        assert!(!error.contains("winpty"), "{error}");
        assert!(verdict("trust", true, false, false, false).is_err());
    }

    /// Git Bash не отличить от канала: отказ называет обходной способ, а не обвиняет в скрипте.
    #[test]
    fn where_the_terminal_cannot_be_detected_the_refusal_names_winpty() {
        let error = verdict("trust", false, false, false, true).unwrap_err();
        assert!(
            error.contains("winpty") && error.contains("mintty"),
            "{error}"
        );
    }

    #[test]
    fn an_agent_child_is_refused_even_with_a_terminal() {
        let error = verdict("secrets bind", true, true, true, false).unwrap_err();
        assert!(error.contains(AGENT_CHILD_ENV), "{error}");
        // Человеку из унаследованного окна сказано, что произошло и как быть.
        assert!(
            error.contains("inherited") && error.contains("unset"),
            "{error}"
        );
        assert!(
            error.contains("Remove-Item") && error.contains("set POOPRUSTEEK"),
            "{error}"
        );
    }
}
