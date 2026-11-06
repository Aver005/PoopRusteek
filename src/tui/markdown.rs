use crate::tui::theme::Theme;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use syntect::easy::HighlightLines;
use syntect::highlighting::{HighlightState, Style as SynStyle, Theme as SynTheme, ThemeSet};
use syntect::parsing::{ParseState, SyntaxReference, SyntaxSet};

/// Подсветка открытого блока кода продолжается с места, где остановилась.
///
/// Пока ответ стримится, весь текст разбирается заново каждый кадр, а блок
/// кода растёт на несколько знаков. Полная подсветка всего блока на каждый
/// токен была самой дорогой частью кадра: здесь хранятся готовые строки и
/// состояние разбора после них, и считаются только новые строки.
/// Ключ — язык, цвет `warning` (им красятся запасные пути) и первая строка:
/// она отличает соседние блоки одного языка в одном сообщении.
static HIGHLIGHT_PROGRESS: LazyLock<Mutex<HashMap<ProgressKey, HighlightProgress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Сколько блоков держим; сверх этого кэш сбрасывается целиком.
const HIGHLIGHT_CACHE_CAP: usize = 32;
/// Блоки больше этого не запоминаем: их строки заняли бы слишком много памяти.
const HIGHLIGHT_CACHE_MAX_BYTES: usize = 256 * 1024;
/// Сколько байт первой строки идёт в ключ.
const PROGRESS_KEY_HEAD_BYTES: usize = 80;

/// Язык, цвет `warning`, первая строка и номер блока в сообщении.
type ProgressKey = (String, Color, String, usize);

/// Что известно о блоке кода после обработки его законченных строк.
struct HighlightProgress {
    /// Текст блока до последнего перевода строки включительно.
    done_text: String,
    done_lines: Vec<Line<'static>>,
    /// Состояние разбора после `done_text`.
    state: (HighlightState, ParseState),
}

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(ThemeSet::load_defaults)
}

fn syntect_to_ratatui(style: SynStyle) -> Style {
    let fg = style.foreground;
    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b))
}

/// Обрезать до `width` клеток, оставив место под многоточие.
fn truncate_cells(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Самая широкая колонка таблицы: дальше ячейка обрезается многоточием.
const TABLE_MAX_CELL_W: usize = 40;

/// Таблица в колонках по ширине содержимого, без заливки. Первая строка — заголовок.
fn table_lines(rows: &[Vec<String>], theme: &Theme) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let mut widths = vec![1usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index]
                .max(UnicodeWidthStr::width(cell.as_str()))
                .min(TABLE_MAX_CELL_W);
        }
    }
    let pad = |text: &str, width: usize| -> String {
        let shown = if UnicodeWidthStr::width(text) > width {
            truncate_cells(text, width)
        } else {
            text.to_string()
        };
        let fill = width.saturating_sub(UnicodeWidthStr::width(shown.as_str()));
        format!("{shown}{}", " ".repeat(fill))
    };
    let divider = Style::default().fg(theme.text_dim);
    let mut lines = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let mut spans = vec![Span::raw("  ")];
        for (column, width) in widths.iter().enumerate() {
            let cell = row.get(column).map_or("", String::as_str);
            let style = if row_index == 0 {
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg)
            };
            if column > 0 {
                spans.push(Span::styled(" \u{2502} ", divider));
            }
            spans.push(Span::styled(pad(cell, *width), style));
        }
        lines.push(Line::from(spans));
        if row_index == 0 {
            let rule: Vec<String> = widths.iter().map(|w| "\u{2500}".repeat(*w)).collect();
            lines.push(Line::from(Span::styled(
                format!("  {}", rule.join("\u{2500}\u{253C}\u{2500}")),
                divider,
            )));
        }
    }
    lines
}

pub fn render_markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);

    let glyph = crate::tui::glyphs::get();
    let quote_prefix = Span::styled(
        format!("  {} ", glyph.quote_bar),
        Style::default().fg(theme.text_dim),
    );
    let parser = Parser::new_ext(text, options);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_blocks = 0usize;
    let mut code_block_lang = String::new();
    let mut code_buffer: Vec<String> = Vec::new();
    // Таблица собирается целиком: ширина колонок известна только в конце.
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut table_row: Vec<String> = Vec::new();
    let mut table_cell = String::new();
    let mut quote_depth = 0usize;
    // Stack of active emphasis/strong/strikethrough modifiers, pushed on
    // `Start` and popped on the matching `End` so nested spans (e.g. bold
    // inside italic) revert to the right accumulated style rather than
    // clearing everything.
    let mut style_stack: Vec<Modifier> = Vec::new();
    // Стиль заголовка действует на его текст; решётки в тексте не рисуются.
    let mut heading_style: Option<Style> = None;

    for event in parser {
        let quote = (quote_depth > 0).then_some(&quote_prefix);
        match event {
            Event::Start(tag) => match tag {
                Tag::Table(_) => {
                    in_table = true;
                    table_rows.clear();
                    flush_line(&mut lines, &mut current_line, quote);
                }
                Tag::TableHead | Tag::TableRow => table_row.clear(),
                Tag::Heading { level, .. } => {
                    use pulldown_cmark::HeadingLevel;
                    let style = match level {
                        HeadingLevel::H1 => Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        HeadingLevel::H2 => Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                        _ => Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                    };
                    heading_style = Some(style);
                }
                Tag::CodeBlock(lang) => {
                    in_code_block = true;
                    code_block_lang = match lang {
                        pulldown_cmark::CodeBlockKind::Fenced(cow) => cow.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                    code_buffer.clear();
                    flush_line(&mut lines, &mut current_line, quote);
                }
                Tag::List(_) => {}
                Tag::Item => {
                    current_line.push(Span::styled(
                        "  \u{2022} ",
                        Style::default().fg(theme.accent),
                    ));
                }
                Tag::Emphasis => {
                    style_stack.push(Modifier::ITALIC);
                }
                Tag::Strong => {
                    style_stack.push(Modifier::BOLD);
                }
                Tag::Strikethrough => {
                    style_stack.push(Modifier::CROSSED_OUT);
                }
                Tag::BlockQuote(_) => {
                    flush_line_if_text(&mut lines, &mut current_line, quote);
                    quote_depth += 1;
                }
                Tag::Link { dest_url, .. } => {
                    current_line.push(Span::styled(
                        format!("[{dest_url}]"),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Table => {
                    in_table = false;
                    lines.extend(table_lines(&table_rows, theme));
                    table_rows.clear();
                    flush_line(&mut lines, &mut current_line, quote);
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if in_table {
                        table_rows.push(std::mem::take(&mut table_row));
                    }
                }
                TagEnd::TableCell => {
                    if in_table {
                        table_row.push(std::mem::take(&mut table_cell));
                    }
                }
                TagEnd::Heading(_) => {
                    heading_style = None;
                    flush_line(&mut lines, &mut current_line, quote);
                }
                TagEnd::CodeBlock => {
                    if !code_buffer.is_empty() {
                        let highlighted = highlight_code(
                            &code_buffer.join("\n"),
                            &code_block_lang,
                            theme,
                            code_blocks,
                        );
                        code_blocks += 1;
                        lines.extend(code_block_lines(&code_block_lang, highlighted, theme));
                    }
                    in_code_block = false;
                    code_block_lang.clear();
                    code_buffer.clear();
                    flush_line(&mut lines, &mut current_line, quote);
                }
                TagEnd::Item => {
                    flush_line(&mut lines, &mut current_line, quote);
                }
                TagEnd::Paragraph => {
                    flush_line(&mut lines, &mut current_line, quote);
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buffer.push(text.to_string());
                } else if in_table {
                    table_cell.push_str(&text);
                } else {
                    let mut style = heading_style.unwrap_or_else(|| Style::default().fg(theme.fg));
                    for modifier in &style_stack {
                        style = style.add_modifier(*modifier);
                    }
                    current_line.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                if in_table {
                    table_cell.push_str(&code);
                } else {
                    current_line.push(Span::styled(
                        format!(" {code} "),
                        Style::default().fg(theme.warning).bg(theme.tool_bg),
                    ));
                }
            }
            Event::TaskListMarker(checked) => {
                // Маркер списка заменяется чекбоксом: `[x]` в Unicode — `☑`.
                current_line.pop();
                let mark = if checked {
                    glyph.checked
                } else {
                    glyph.unchecked
                };
                current_line.push(Span::styled(
                    format!("  {mark} "),
                    Style::default().fg(theme.accent),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut lines, &mut current_line, quote);
            }
            Event::Rule => {
                flush_line(&mut lines, &mut current_line, quote);
                let rule = "\u{2500}".repeat(40);
                lines.push(Line::from(vec![Span::styled(
                    rule,
                    Style::default().fg(theme.text_dim),
                )]));
            }
            Event::Html(html) => {
                current_line.push(Span::styled(
                    html.to_string(),
                    Style::default().fg(theme.text_dim),
                ));
            }
            _ => {}
        }
    }

    if !current_line.is_empty() {
        let quote = (quote_depth > 0).then_some(&quote_prefix);
        flush_line(&mut lines, &mut current_line, quote);
    }

    lines
}

/// Блок кода: подпись языка и заливка цветом панели на всю ширину строки.
fn code_block_lines(
    lang: &str,
    highlighted: Vec<Line<'static>>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let panel = Style::default().bg(theme.panel);
    let mut out = Vec::with_capacity(highlighted.len() + 1);
    if !lang.is_empty() {
        out.push(
            Line::from(Span::styled(
                format!("  {lang}"),
                Style::default().fg(theme.text_dim),
            ))
            .style(panel),
        );
    }
    out.extend(highlighted.into_iter().map(|line| line.style(panel)));
    out
}

fn plain_code_lines(code: &str, theme: &Theme) -> Vec<Line<'static>> {
    code.lines()
        .map(|line| {
            Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.warning),
            ))
        })
        .collect()
}

/// Одна строка кода: отступ и куски цвета от syntect.
fn highlight_line(
    highlighter: &mut HighlightLines<'_>,
    line: &str,
    syntax_set: &SyntaxSet,
    fallback: Color,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  ", Style::default())];
    match highlighter.highlight_line(line, syntax_set) {
        Ok(pieces) => {
            for (style, text) in pieces {
                spans.push(Span::styled(text.to_string(), syntect_to_ratatui(style)));
            }
        }
        Err(_) => spans.push(Span::styled(
            line.to_string(),
            Style::default().fg(fallback),
        )),
    }
    Line::from(spans)
}

/// Продолжить подсветку блока `code` с `progress` (его текст — начало `code`).
/// Возвращает все строки блока; законченные запоминаются в `progress`,
/// незаконченная последняя считается заново и не запоминается.
fn continue_highlight(
    code: &str,
    progress: &mut HighlightProgress,
    syntax_set: &SyntaxSet,
    syn_theme: &SynTheme,
    fallback: Color,
) -> Vec<Line<'static>> {
    let rest = &code[progress.done_text.len()..];
    let state = (progress.state.0.clone(), progress.state.1.clone());
    let mut highlighter = HighlightLines::from_state(syn_theme, state.0, state.1);
    let mut consumed = 0;
    while let Some(newline) = rest[consumed..].find('\n') {
        let line = rest[consumed..consumed + newline].trim_end_matches('\r');
        progress
            .done_lines
            .push(highlight_line(&mut highlighter, line, syntax_set, fallback));
        consumed += newline + 1;
    }
    progress.done_text.push_str(&rest[..consumed]);
    let (highlight_state, parse_state) = highlighter.state();
    let mut lines = progress.done_lines.clone();
    let tail = rest[consumed..].trim_end_matches('\r');
    if !tail.is_empty() {
        // Хвост без перевода строки ещё растёт: считаем его на копии состояния.
        let mut tail_highlighter =
            HighlightLines::from_state(syn_theme, highlight_state.clone(), parse_state.clone());
        lines.push(highlight_line(
            &mut tail_highlighter,
            tail,
            syntax_set,
            fallback,
        ));
    }
    progress.state = (highlight_state, parse_state);
    lines
}

fn fresh_progress(syntax: &SyntaxReference, syn_theme: &SynTheme) -> HighlightProgress {
    HighlightProgress {
        done_text: String::new(),
        done_lines: Vec::new(),
        state: HighlightLines::new(syntax, syn_theme).state(),
    }
}

/// Номер блока различает два блока с одной первой строкой (`cargo test` в двух
/// bash-блоках): без него они вытесняли бы прогресс друг друга на каждом кадре.
fn progress_key(code: &str, lang: &str, theme: &Theme, ordinal: usize) -> ProgressKey {
    let head = code.lines().next().unwrap_or("");
    let head = crate::util::truncate_at_char_boundary(head, PROGRESS_KEY_HEAD_BYTES);
    (lang.to_string(), theme.warning, head.to_string(), ordinal)
}

fn highlight_code(code: &str, lang: &str, theme: &Theme, ordinal: usize) -> Vec<Line<'static>> {
    if lang.is_empty() {
        return plain_code_lines(code, theme);
    }

    let ss = syntax_set();
    let ts = theme_set();
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let syn_theme = ts
        .themes
        .get("base16-ocean.dark")
        .or_else(|| ts.themes.get("InspiredGitHub"))
        .or_else(|| ts.themes.values().next());
    let Some(syn_theme) = syn_theme else {
        return plain_code_lines(code, theme);
    };

    if code.len() > HIGHLIGHT_CACHE_MAX_BYTES {
        let mut progress = fresh_progress(syntax, syn_theme);
        return continue_highlight(code, &mut progress, ss, syn_theme, theme.warning);
    }

    // Пока нет ни одной законченной строки, запоминать нечего, а первая строка
    // ещё растёт: ключ менялся бы с каждым токеном и забивал кэш мусором.
    if !code.contains('\n') {
        let mut progress = fresh_progress(syntax, syn_theme);
        return continue_highlight(code, &mut progress, ss, syn_theme, theme.warning);
    }
    let key = progress_key(code, lang, theme, ordinal);
    // Берём запись из карты на время подсветки: замок не держим на разборе.
    let stored = HIGHLIGHT_PROGRESS
        .lock()
        .ok()
        .and_then(|mut cache| cache.remove(&key));
    let mut progress = match stored {
        Some(progress) if code.starts_with(&progress.done_text) => progress,
        _ => fresh_progress(syntax, syn_theme),
    };
    let lines = continue_highlight(code, &mut progress, ss, syn_theme, theme.warning);
    if let Ok(mut cache) = HIGHLIGHT_PROGRESS.lock() {
        if cache.len() >= HIGHLIGHT_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, progress);
    }
    lines
}

/// Закрыть строку. В цитате перед ней ставится полоса.
fn flush_line(
    lines: &mut Vec<Line<'static>>,
    current_line: &mut Vec<Span<'static>>,
    quote: Option<&Span<'static>>,
) {
    if current_line.is_empty() {
        lines.push(Line::from(""));
        return;
    }
    let mut spans = std::mem::take(current_line);
    if let Some(prefix) = quote {
        spans.insert(0, prefix.clone());
    }
    lines.push(Line::from(spans));
}

/// Закрыть строку только если в ней что-то накоплено (границы блоков).
fn flush_line_if_text(
    lines: &mut Vec<Line<'static>>,
    current_line: &mut Vec<Span<'static>>,
    quote: Option<&Span<'static>>,
) {
    if !current_line.is_empty() {
        flush_line(lines, current_line, quote);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_span_containing<'a>(
        lines: &'a [Line<'static>],
        needle: &str,
    ) -> Option<&'a Span<'static>> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
    }

    #[test]
    fn emphasis_applies_italic_modifier() {
        let theme = Theme::default_dark();
        let lines = render_markdown("*italic*", &theme);
        let span = find_span_containing(&lines, "italic").expect("italic span present");
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn strong_applies_bold_modifier() {
        let theme = Theme::default_dark();
        let lines = render_markdown("**bold**", &theme);
        let span = find_span_containing(&lines, "bold").expect("bold span present");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn strikethrough_applies_crossed_out_modifier() {
        let theme = Theme::default_dark();
        let lines = render_markdown("~~strike~~", &theme);
        let span = find_span_containing(&lines, "strike").expect("strike span present");
        assert!(span.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn nested_strong_inside_emphasis_carries_both_modifiers() {
        let theme = Theme::default_dark();
        let lines = render_markdown("*outer **inner** outer*", &theme);
        let inner = find_span_containing(&lines, "inner").expect("inner span present");
        assert!(inner.style.add_modifier.contains(Modifier::ITALIC));
        assert!(inner.style.add_modifier.contains(Modifier::BOLD));

        // Text outside the nested Strong should still be italic-only.
        let outer = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("outer") && !s.content.contains("inner"))
            .expect("outer span present");
        assert!(outer.style.add_modifier.contains(Modifier::ITALIC));
        assert!(!outer.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn plain_text_has_no_emphasis_modifiers() {
        let theme = Theme::default_dark();
        let lines = render_markdown("plain text", &theme);
        let span = find_span_containing(&lines, "plain").expect("plain span present");
        assert!(!span.style.add_modifier.contains(Modifier::ITALIC));
        assert!(!span.style.add_modifier.contains(Modifier::BOLD));
        assert!(!span.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    const RUST_SNIPPET: &str =
        "fn main() {\n    let answer = 42;\n    println!(\"{answer}\");\n}\n";

    /// Подсветка «с нуля» для сравнения: пустой прогресс, без кэша.
    fn highlight_fresh(code: &str) -> Vec<Line<'static>> {
        let (ss, ts) = (syntax_set(), theme_set());
        let syntax = ss.find_syntax_by_token("rust").expect("rust syntax");
        let syn_theme = ts.themes.get("base16-ocean.dark").expect("syntect theme");
        let mut progress = fresh_progress(syntax, syn_theme);
        continue_highlight(code, &mut progress, ss, syn_theme, Color::Red)
    }

    #[test]
    fn a_growing_block_highlights_exactly_like_one_pass() {
        let theme = Theme::default_dark();
        // Растим блок по три знака, как стрим; на каждом шаге результат
        // обязан совпасть с подсветкой всего текста за один проход.
        let mut end = 0;
        while end < RUST_SNIPPET.len() {
            end = (end + 3).min(RUST_SNIPPET.len());
            let prefix = &RUST_SNIPPET[..end];
            assert_eq!(
                highlight_code(prefix, "rust", &theme, 0),
                highlight_fresh(prefix),
                "prefix of {end} bytes"
            );
        }
    }

    #[test]
    fn repeating_the_same_block_gives_the_same_lines() {
        let theme = Theme::default_dark();
        let first = highlight_code(RUST_SNIPPET, "rust", &theme, 0);
        let second = highlight_code(RUST_SNIPPET, "rust", &theme, 0);
        assert_eq!(first, second);
        assert_eq!(first.len(), 4);
    }

    #[test]
    fn a_rewritten_block_with_the_same_first_line_is_not_served_stale() {
        let theme = Theme::default_dark();
        let _ = highlight_code("fn a() {\n    1\n}\n", "rust", &theme, 0);
        let rewritten = "fn a() {\n    2\n}\n";
        assert_eq!(
            highlight_code(rewritten, "rust", &theme, 0),
            highlight_fresh(rewritten)
        );
    }

    /// Два блока с одной первой строкой получают разные ключи и не вытесняют друг друга.
    #[test]
    fn two_blocks_with_the_same_first_line_do_not_share_progress() {
        let theme = Theme::default_dark();
        let code = "cargo test\nls\n";
        assert_ne!(
            progress_key(code, "bash", &theme, 0),
            progress_key(code, "bash", &theme, 1)
        );
        // Вывод при этом тот же, что у подсветки с нуля.
        let text = format!("```bash\n{code}```\n\n```bash\n{code}```\n");
        let first = render_markdown(&text, &theme);
        let second = render_markdown(&text, &theme);
        assert_eq!(first, second);
    }
}
