//! Окно подтверждения инструмента. Человек разрешает то, что видит, поэтому
//! аргументы переносятся по ширине и окно растёт под них, а не режет хвост.
//! Размеры считает [`viewport`]: его зовут и отрисовка, и клавиши прокрутки.

use super::popup::{center_popup, draw_popup, modal_block, separator_line};
use super::util::highlight_json;
use crate::app::view_state::Grant;
use crate::tui::theme::Theme;
use crate::tui::wrap::{wrap_spans, wrap_words};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Самое широкое окно: дальше строки трудно читать.
const MAX_POPUP_W: u16 = 100;
/// Ширина, на которой считаем, пока терминал ещё не сообщил размер.
const FALLBACK_COLS: u16 = 80;
const FALLBACK_ROWS: u16 = 24;
/// Рамка (2) + строка инструмента + пустая + «Arguments» + разделитель + клавиши.
const FIXED_ROWS: usize = 7;
/// Левый отступ в окне и запас справа.
const BODY_INSET: usize = 4;

/// Что нужно знать о запросе, чтобы посчитать окно.
pub(crate) struct ApprovalShape<'a> {
    pub arguments: &'a str,
    pub tool_name: &'a str,
    pub has_origin: bool,
    pub scope: Option<&'a crate::whitelist::Scope>,
}

/// Размеры окна и окна прокрутки аргументов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ApprovalViewport {
    pub popup_w: u16,
    pub popup_h: u16,
    pub body_w: usize,
    /// Сколько строк аргументов видно сразу.
    pub shown_rows: usize,
    /// Сколько строк у аргументов после переноса.
    pub total_rows: usize,
}

impl ApprovalViewport {
    /// Дальше этой строки прокручивать нечего.
    pub(crate) fn max_scroll(&self) -> usize {
        self.total_rows.saturating_sub(self.shown_rows)
    }
}

/// Строки подписей вариантов разрешения после переноса.
fn grant_rows(shape: &ApprovalShape<'_>, body_w: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for option in Grant::all(shape.scope.is_some()) {
        let label = option.label(shape.tool_name, shape.scope);
        rows.extend(wrap_words(&label, body_w.saturating_sub(2)));
    }
    rows
}

/// Аргументы, разбитые по ширине тела окна, с подсветкой.
fn wrapped_arguments(arguments: &str, body_w: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in highlight_json(arguments, theme) {
        let indent = line.spans.first().map_or(0, |span| {
            span.content.chars().take_while(|c| *c == ' ').count()
        });
        for row in wrap_spans(&line.spans, body_w, indent + 2) {
            rows.push(Line::from(row));
        }
    }
    if rows.is_empty() {
        rows.push(Line::from(Span::styled(
            "(no arguments)",
            Style::default().fg(theme.text_dim),
        )));
    }
    rows
}

/// Размеры окна для терминала `cols`×`rows`. Нулевой размер означает «ещё не знаем».
pub(crate) fn viewport(cols: u16, rows: u16, shape: &ApprovalShape<'_>) -> ApprovalViewport {
    let cols = if cols == 0 { FALLBACK_COLS } else { cols };
    let rows = if rows == 0 { FALLBACK_ROWS } else { rows };
    let popup_w = if cols <= 44 {
        cols
    } else {
        (cols - 4).min(MAX_POPUP_W)
    };
    let body_w = (popup_w as usize).saturating_sub(BODY_INSET + 2).max(8);
    let total_rows = wrapped_arguments(shape.arguments, body_w, &Theme::default_dark()).len();
    let chrome = FIXED_ROWS + usize::from(shape.has_origin) + grant_rows(shape, body_w).len();
    let max_popup_h = (rows as usize).saturating_sub(2).max(chrome + 2);
    let room = max_popup_h.saturating_sub(chrome);
    let shown_rows = if total_rows <= room {
        total_rows
    } else {
        // Одну строку отдаём под индикатор прокрутки.
        room.saturating_sub(1).max(1)
    };
    let indicator = usize::from(total_rows > shown_rows);
    ApprovalViewport {
        popup_w,
        popup_h: (chrome + shown_rows + indicator).min(max_popup_h) as u16,
        body_w,
        shown_rows,
        total_rows,
    }
}

/// Команды, после которых стоит остановиться и перечитать вызов.
const DANGER_COMMANDS: [&str; 14] = [
    "rm",
    "rmdir",
    "del",
    "erase",
    "rd",
    "remove-item",
    "mkfs",
    "dd",
    "sudo",
    "shutdown",
    "reboot",
    "format",
    "kill",
    "taskkill",
];
/// Флаги, которые делают вызов необратимым или грубым.
const DANGER_FLAGS: [&str; 8] = [
    "--force",
    "-force",
    "-rf",
    "-fr",
    "--hard",
    "--no-preserve-root",
    "-recurse",
    "--delete",
];

/// Слово из команды опасное: сама команда (без пути и знаков вокруг) или флаг.
fn is_danger_token(token: &str) -> bool {
    let word = token.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '"' | '\'' | '`')
    });
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_lowercase();
    let name = name.trim_end_matches(".exe");
    DANGER_COMMANDS.contains(&name) || DANGER_FLAGS.contains(&name)
}

/// Короткий итог вызова для заголовка: команда, путь или запрос. Секреты
/// замаскированы (полный текст виден ниже, в аргументах).
fn call_summary(arguments: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) if value.is_object() => crate::agent::tool_call_summary(&value),
        _ => crate::util::one_line_label(
            arguments
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(""),
            120,
        ),
    }
}

/// Заголовок окна: `⚠ Tool call · bash · <итог>`; опасные слова итога красные и жирные.
fn title_line(tool_name: &str, arguments: &str, popup_w: usize, theme: &Theme) -> Line<'static> {
    let warn = Style::default()
        .fg(theme.warning)
        .add_modifier(Modifier::BOLD);
    let danger = Style::default()
        .fg(theme.error)
        .add_modifier(Modifier::BOLD);
    let head = format!(
        " {} Tool call \u{00B7} {tool_name} ",
        crate::tui::glyphs::get().warn
    );
    let room = popup_w
        .saturating_sub(UnicodeWidthStr::width(head.as_str()) + 4)
        .max(8);
    let summary = super::util::truncate(&call_summary(arguments), room);
    let mut spans = vec![Span::styled(head, warn)];
    if !summary.is_empty() {
        spans.push(Span::styled("\u{00B7} ", warn));
        for token in summary.split_inclusive(' ') {
            let style = if is_danger_token(token) { danger } else { warn };
            spans.push(Span::styled(token.to_string(), style));
        }
        spans.push(Span::styled(" ", warn));
    }
    Line::from(spans)
}

/// Состояние окна одним аргументом.
pub(super) struct ToolApprovalView<'a> {
    pub tool_name: &'a str,
    pub arguments: &'a str,
    pub origin: Option<&'a str>,
    pub scroll_offset: usize,
    pub grant: Grant,
    pub scope: Option<&'a crate::whitelist::Scope>,
}

pub(super) fn render_tool_approval(
    frame: &mut Frame,
    area: Rect,
    view: ToolApprovalView<'_>,
    theme: &Theme,
) {
    let ToolApprovalView {
        tool_name,
        arguments,
        origin,
        scroll_offset,
        grant,
        scope,
    } = view;
    let shape = ApprovalShape {
        arguments,
        tool_name,
        has_origin: origin.is_some(),
        scope,
    };
    let vp = viewport(area.width, area.height, &shape);
    let popup_area = center_popup(
        area,
        vp.popup_w.min(area.width),
        vp.popup_h.min(area.height),
    );
    let block = modal_block(
        title_line(tool_name, arguments, vp.popup_w as usize, theme),
        theme.warning,
        theme,
    );
    let inner = block.inner(popup_area);
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", crate::tui::glyphs::get().tool),
            Style::default().fg(theme.warning),
        ),
        Span::styled("Tool:", Style::default().fg(theme.text_dim)),
        Span::styled(" ", Style::default().fg(theme.fg)),
        Span::styled(
            tool_name.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    // Кто спрашивает — своей строкой, вне аргументов: подпись из текста модели
    // не должна рисовать в окне поддельные аргументы.
    if let Some(origin) = origin {
        lines.push(Line::from(vec![
            Span::styled("    From: ", Style::default().fg(theme.text_dim)),
            Span::styled(origin.to_string(), Style::default().fg(theme.warning)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Arguments:",
        Style::default().fg(theme.text_dim),
    )));

    let rows = wrapped_arguments(arguments, vp.body_w, theme);
    let start = scroll_offset.min(vp.max_scroll());
    let end = (start + vp.shown_rows).min(rows.len());
    for row in &rows[start..end] {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(row.spans.iter().cloned());
        lines.push(Line::from(spans));
    }
    if vp.total_rows > vp.shown_rows {
        let hidden = vp.total_rows - vp.shown_rows;
        lines.push(Line::from(Span::styled(
            format!(
                "  \u{2191}\u{2193} rows {}-{} of {} ({hidden} more)",
                start + 1,
                end,
                vp.total_rows
            ),
            Style::default().fg(theme.text_dim),
        )));
    }

    lines.push(separator_line(inner_w, theme));
    // Все варианты списком, а не только текущий: человек, видящий одну
    // строку «just this once», не знает, что там есть что-то ещё.
    for option in Grant::all(scope.is_some()) {
        let current = option == grant;
        let style = match (current, option) {
            (false, _) => Style::default().fg(theme.text_dim),
            (true, Grant::Tool) => Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
            (true, _) => Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
        };
        let label = option.label(tool_name, scope);
        for (index, part) in wrap_words(&label, vp.body_w.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let mark = match (index, current) {
                (0, true) => crate::tui::glyphs::get().radio_on,
                (0, false) => crate::tui::glyphs::get().radio_off,
                _ => " ",
            };
            lines.push(Line::from(Span::styled(format!("  {mark} {part}"), style)));
        }
    }
    lines.push(key_hints(theme));

    draw_popup(frame, popup_area, inner, block, lines, theme);
}

/// Подсказка клавиш; считается по ширине в клетках, чтобы влезть в самое узкое окно.
fn key_hints(theme: &Theme) -> Line<'static> {
    let key = |text: &'static str, color| {
        Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let hint = |text: &'static str| Span::styled(text, Style::default().fg(theme.text_dim));
    Line::from(vec![
        Span::raw("  "),
        key("Y", theme.success),
        hint(" allow  "),
        key("N", theme.error),
        hint(" deny  "),
        key("A", theme.accent),
        hint(" change  "),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(theme.accent_soft)),
        hint(" scroll"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn shape(arguments: &str) -> ApprovalShape<'_> {
        ApprovalShape {
            arguments,
            tool_name: "bash",
            has_origin: false,
            scope: None,
        }
    }

    const LONG_COMMAND: &str = "command: cargo test --bin pooprusteek items::range -- --nocapture && git diff --stat && echo the-tail-must-be-visible";

    #[test]
    fn a_long_command_is_wrapped_not_cut() {
        let vp = viewport(80, 24, &shape(LONG_COMMAND));
        assert!(vp.total_rows > 1, "{vp:?}");
        let rows = wrapped_arguments(LONG_COMMAND, vp.body_w, &Theme::default_dark());
        let text: String = rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("the-tail-must-be-visible"), "{text}");
        for row in &rows {
            let width: usize = row
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                width <= vp.body_w,
                "row of {width} cells, body {}",
                vp.body_w
            );
        }
    }

    #[test]
    fn dangerous_words_are_recognised_without_false_alarms() {
        for token in [
            "rm",
            "-rf",
            "--force",
            "sudo",
            "/bin/rm",
            "Remove-Item",
            "rm;",
            "\"dd\"",
            "RM.EXE",
        ] {
            assert!(is_danger_token(token), "{token}");
        }
        for token in [
            "cargo", "test", "--bin", "form", "remote", "-f", "firm", "--forced",
        ] {
            assert!(!is_danger_token(token), "{token}");
        }
    }

    #[test]
    fn the_title_names_the_tool_and_marks_the_dangerous_words() {
        let theme = Theme::default_dark();
        let arguments = "{\n  \"command\": \"rm -rf node_modules && echo done\"\n}";
        let title = title_line("bash", arguments, 100, &theme);
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("bash") && text.contains("rm -rf node_modules"),
            "{text}"
        );
        let danger_style = |word: &str| {
            title
                .spans
                .iter()
                .find(|s| s.content.trim() == word)
                .map(|s| s.style.fg)
        };
        assert_eq!(danger_style("rm"), Some(Some(theme.error)));
        assert_eq!(danger_style("-rf"), Some(Some(theme.error)));
        assert_eq!(danger_style("echo"), Some(Some(theme.warning)));
    }

    #[test]
    fn a_long_summary_is_cut_to_the_window_and_a_secret_stays_masked() {
        let theme = Theme::default_dark();
        let long = format!(
            "{{\"command\": \"deploy --token s3cr3t {}\"}}",
            "x".repeat(300)
        );
        let title = title_line("bash", &long, 60, &theme);
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(UnicodeWidthStr::width(text.as_str()) <= 60, "{text}");
        assert!(!text.contains("s3cr3t"), "{text}");
    }

    #[test]
    fn non_json_arguments_still_give_a_title() {
        let theme = Theme::default_dark();
        let title = title_line("write", "path: /tmp/x.txt\n\n+ hello", 80, &theme);
        let text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("path: /tmp/x.txt"), "{text}");
    }

    #[test]
    fn the_window_grows_to_fit_the_command_when_there_is_room() {
        let vp = viewport(120, 40, &shape(LONG_COMMAND));
        assert_eq!(vp.shown_rows, vp.total_rows);
        assert_eq!(vp.max_scroll(), 0);
    }

    #[test]
    fn the_window_fits_the_screen_and_scrolls_the_rest() {
        let huge = "command: ".to_string() + &"x".repeat(5_000);
        for (cols, rows) in [(60u16, 20u16), (80, 24), (120, 40)] {
            let vp = viewport(cols, rows, &shape(&huge));
            assert!(vp.popup_h <= rows, "{cols}x{rows}: {vp:?}");
            assert!(vp.popup_w <= cols, "{cols}x{rows}: {vp:?}");
            assert!(vp.max_scroll() > 0, "{cols}x{rows}: {vp:?}");
        }
    }

    #[test]
    fn unknown_terminal_size_falls_back_instead_of_collapsing() {
        let vp = viewport(0, 0, &shape(LONG_COMMAND));
        assert!(vp.popup_w >= 40 && vp.popup_h > 6, "{vp:?}");
    }
}
