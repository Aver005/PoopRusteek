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

/// Чистое решение, чтобы его можно было проверить без терминала.
fn verdict(what: &str, stdin_tty: bool, stderr_tty: bool, agent_child: bool) -> Result<(), String> {
    if agent_child {
        return Err(format!(
            "{what} must be run by a person in their own terminal, not from an agent tool \
             ({AGENT_CHILD_ENV} is set)."
        ));
    }
    if !(stdin_tty && stderr_tty) {
        return Err(format!(
            "{what} needs a real terminal and a person at it (stdin and stderr must be a TTY); \
             piped or scripted runs are refused."
        ));
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
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_at_a_terminal_passes() {
        assert!(verdict("trust", true, true, false).is_ok());
    }

    #[test]
    fn a_pipe_on_either_stream_is_refused() {
        let error = verdict("trust", false, true, false).unwrap_err();
        assert!(error.contains("real terminal"), "{error}");
        assert!(verdict("trust", true, false, false).is_err());
    }

    #[test]
    fn an_agent_child_is_refused_even_with_a_terminal() {
        let error = verdict("secrets bind", true, true, true).unwrap_err();
        assert!(error.contains(AGENT_CHILD_ENV), "{error}");
    }
}
