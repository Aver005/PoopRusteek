//! Блок результата инструмента в переписке: статус по настоящему итогу и хвост вывода.
//! Успех команды определяет код выхода, а не то, что инструмент «отработал».

use crate::provider::ChatMessage;
use crate::tui::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Строк тела у успешного результата.
const PREVIEW_LINES: usize = 3;
/// Хвост упавшей команды: сводка теста обычно в конце.
const FAILED_TAIL_LINES: usize = 8;
/// Потолок строк ошибки самого инструмента.
const ERROR_MAX_LINES: usize = 40;

/// Чем кончилась команда, если вывод это сообщает.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Exit {
    Code(i32),
    Signal,
}

/// Итог блока: упал ли он на самом деле и что показать.
#[derive(Debug, PartialEq)]
pub(super) struct Shape<'a> {
    pub failed: bool,
    pub exit: Option<Exit>,
    pub body: Vec<&'a str>,
}

fn is_shell(name: &str) -> bool {
    crate::agent::is_shell_tool(name)
}

/// Отделяет подпись `[exit code N]` от вывода, которую ставит оболочка.
fn split_exit(body: &str) -> (Vec<&str>, Option<Exit>) {
    let mut lines: Vec<&str> = body.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let exit = lines.last().and_then(|last| {
        let last = last.trim();
        if last == "[terminated by a signal]" {
            return Some(Exit::Signal);
        }
        last.strip_prefix("[exit code ")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|code| code.parse().ok())
            .map(Exit::Code)
    });
    if exit.is_some() {
        lines.pop();
    }
    (lines, exit)
}

pub(super) fn shape(msg: &ChatMessage) -> Shape<'_> {
    let name = msg.name.as_deref().unwrap_or("");
    let (body, exit) = if is_shell(name) {
        split_exit(msg.visible_content())
    } else {
        (msg.visible_content().lines().collect(), None)
    };
    Shape {
        failed: msg.tool_error || exit.is_some(),
        exit,
        body,
    }
}

fn exit_label(exit: Option<Exit>) -> Option<String> {
    match exit? {
        Exit::Code(code) => Some(format!("exit {code}")),
        Exit::Signal => Some("killed".to_string()),
    }
}

pub(super) fn tool_lines(msg: &ChatMessage, theme: &Theme) -> Vec<Line<'static>> {
    let glyph = crate::tui::glyphs::get();
    let name = msg.name.as_deref().unwrap_or("unknown");
    let view = shape(msg);
    let status_color = if view.failed {
        theme.error
    } else {
        theme.success
    };
    let bg = theme.bg;

    let mut head = vec![
        Span::styled(
            format!(" {} ", glyph.outcome(view.failed)),
            Style::default()
                .fg(status_color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(label) = exit_label(view.exit) {
        head.push(Span::styled(
            format!("  {label}"),
            Style::default().fg(theme.error).bg(bg),
        ));
    } else if msg.tool_error {
        head.push(Span::styled(
            " failed",
            Style::default().fg(theme.error).bg(bg),
        ));
    }
    let mut lines = vec![Line::from(head)];

    let body = &view.body;
    if body.iter().all(|l| l.trim().is_empty()) {
        lines.push(dim_line("   (no output)".to_string(), theme));
        return lines;
    }
    let total = body.len();
    let tail = is_shell(name);
    // Команда падает с `is_error`, поэтому хвост нужен и ей: итог теста стоит в конце.
    let shown = match (tail, view.failed) {
        (true, true) => total.min(FAILED_TAIL_LINES),
        (false, _) if msg.tool_error => total.min(ERROR_MAX_LINES),
        _ => total.min(PREVIEW_LINES),
    };
    let text_color = if view.failed {
        theme.error
    } else {
        theme.text_soft
    };
    // Команды показывают конец вывода, остальные инструменты — начало.
    let (skipped_before, window) = if tail {
        (total - shown, &body[total - shown..])
    } else {
        (0, &body[..shown])
    };
    if skipped_before > 0 {
        lines.push(dim_line(
            format!("   \u{2026} {skipped_before} earlier lines"),
            theme,
        ));
    }
    for text in window {
        lines.push(Line::from(vec![
            Span::styled("   \u{2502} ", Style::default().fg(theme.text_dim).bg(bg)),
            Span::styled((*text).to_string(), Style::default().fg(text_color).bg(bg)),
        ]));
    }
    if !tail && total > shown {
        lines.push(dim_line(
            format!("   \u{2026} {} more lines", total - shown),
            theme,
        ));
    }
    lines
}

fn dim_line(text: String, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default().fg(theme.text_dim).bg(theme.bg),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, content: &str, error: bool) -> ChatMessage {
        ChatMessage::tool_with_display("id", name, content, content, error)
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Сообщение так, как его собирает рабочий код: `is_error` и лента из `result_display`.
    fn real(name: &str, content: &str, error: bool) -> ChatMessage {
        let display = crate::agent::result_display(name, content, false);
        ChatMessage::tool_with_display("id", name, content, &display, error)
    }

    /// Длинный вывод упавшего теста: итог и код выхода стоят далеко за первыми 200 байтами.
    fn failing_cargo_output() -> String {
        let mut out = String::from("running 40 tests\n");
        for n in 0..40 {
            out.push_str(&format!("test module::case_{n} ... ok\n"));
        }
        out.push_str("test module::broken ... FAILED\n");
        out.push_str("test result: FAILED. 40 passed; 1 failed\n[exit code 101]");
        out
    }

    #[test]
    fn a_real_failing_command_shows_its_tail_and_exit_code() {
        let output = failing_cargo_output();
        assert!(output.len() > 1_000, "the marker is far beyond 200 bytes");
        let msg = real("bash", &output, true);
        let view = shape(&msg);
        assert_eq!(view.exit, Some(Exit::Code(101)));
        assert!(view.failed);
        let rendered = text(&tool_lines(&msg, &Theme::default()));
        assert!(rendered.contains("exit 101"), "{rendered}");
        assert!(rendered.contains("40 passed; 1 failed"), "{rendered}");
        assert!(rendered.contains("earlier lines"), "{rendered}");
        assert!(
            !rendered.contains("case_0 "),
            "the head is not shown: {rendered}"
        );
    }

    #[test]
    fn a_successful_long_command_counts_lines_of_the_command() {
        let output = (0..50)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = real("bash", &output, false);
        let rendered = text(&tool_lines(&msg, &Theme::default()));
        assert!(rendered.contains("line 49"), "{rendered}");
        assert!(
            rendered.contains("47 earlier lines"),
            "whole output counted: {rendered}"
        );
    }

    #[test]
    fn a_failed_non_shell_tool_still_shows_the_head() {
        let output = (0..60)
            .map(|n| format!("problem {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let msg = ChatMessage::tool_with_display("id", "edit", &output, &output, true);
        let rendered = text(&tool_lines(&msg, &Theme::default()));
        assert!(rendered.contains("problem 0"), "{rendered}");
        assert!(!rendered.contains("problem 59"), "{rendered}");
    }

    #[test]
    fn a_command_with_a_non_zero_exit_is_not_green() {
        let msg = tool(
            "bash",
            "running 3 tests\ntest a ... ok\ntest b ... FAILED\ntest result: FAILED. 2 passed; 1 failed\n[exit code 101]",
            false,
        );
        let view = shape(&msg);
        assert!(view.failed);
        assert_eq!(view.exit, Some(Exit::Code(101)));
        let theme = Theme::default();
        let lines = tool_lines(&msg, &theme);
        let head = &lines[0];
        assert_eq!(head.spans[0].style.fg, Some(theme.error), "the mark is red");
        let rendered = text(&lines);
        assert!(rendered.contains("exit 101"), "{rendered}");
        assert!(
            rendered.contains("2 passed; 1 failed"),
            "the summary is visible: {rendered}"
        );
    }

    #[test]
    fn a_clean_exit_stays_green_and_shows_the_tail() {
        let msg = tool("bash", "one\ntwo\nthree\nfour\nfive", false);
        let view = shape(&msg);
        assert!(!view.failed);
        let theme = Theme::default();
        let lines = tool_lines(&msg, &theme);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.success));
        let rendered = text(&lines);
        assert!(
            rendered.contains("five") && rendered.contains("three"),
            "{rendered}"
        );
        assert!(!rendered.contains("one"), "the head is cut: {rendered}");
        assert!(rendered.contains("2 earlier lines"), "{rendered}");
    }

    #[test]
    fn other_tools_keep_showing_the_head() {
        let msg = tool("read_file", "a\nb\nc\nd\ne", false);
        let rendered = text(&tool_lines(&msg, &Theme::default()));
        assert!(
            rendered.contains('a') && rendered.contains("2 more lines"),
            "{rendered}"
        );
        assert!(!rendered.contains("earlier"), "{rendered}");
    }

    #[test]
    fn a_signal_is_a_failure_too() {
        let msg = tool("bash", "partial\n[terminated by a signal]", false);
        assert_eq!(shape(&msg).exit, Some(Exit::Signal));
        assert!(shape(&msg).failed);
    }

    #[test]
    fn a_footer_inside_the_output_is_not_mistaken_for_an_exit() {
        let msg = tool("bash", "[exit code 7]\nand then it went on fine", false);
        let view = shape(&msg);
        assert!(!view.failed, "only the last line counts");
    }
}
