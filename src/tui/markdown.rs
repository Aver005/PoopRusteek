use crate::tui::theme::Theme;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Memoized syntect output per code block.
///
/// While a message is streaming, its whole text is re-rendered every frame
/// (the message-level cache in `widgets::chat` can only serve finished
/// messages), which used to re-highlight every *completed* code block from
/// scratch each time — syntect dominates `render_markdown` cost. Keyed by
/// code + language + the theme's warning color (the syntect theme itself
/// is fixed; `theme.warning` only styles the fallback paths). Bounded two
/// ways: wiped wholesale past `HIGHLIGHT_CACHE_CAP` entries, and blocks
/// over `HIGHLIGHT_CACHE_MAX_BYTES` are never cached — the live working
/// set is just the streaming message's blocks, since finished messages are
/// served whole from the message cache.
#[expect(
    clippy::type_complexity,
    reason = "internal cache map, not an API surface"
)]
static HIGHLIGHT_CACHE: LazyLock<Mutex<HashMap<(String, String, Color), Vec<Line<'static>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const HIGHLIGHT_CACHE_CAP: usize = 128;
const HIGHLIGHT_CACHE_MAX_BYTES: usize = 64 * 1024;

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

pub fn render_markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(text, options);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_buffer: Vec<String> = Vec::new();
    let mut in_table = false;
    // Stack of active emphasis/strong/strikethrough modifiers, pushed on
    // `Start` and popped on the matching `End` so nested spans (e.g. bold
    // inside italic) revert to the right accumulated style rather than
    // clearing everything.
    let mut style_stack: Vec<Modifier> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Table(_) => {
                    in_table = true;
                    flush_line(&mut lines, &mut current_line);
                }
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
                    let prefix = match level {
                        HeadingLevel::H1 => "# ",
                        HeadingLevel::H2 => "## ",
                        HeadingLevel::H3 => "### ",
                        HeadingLevel::H4 => "#### ",
                        HeadingLevel::H5 => "##### ",
                        HeadingLevel::H6 => "###### ",
                    };
                    current_line.push(Span::styled(prefix.to_string(), style));
                }
                Tag::CodeBlock(lang) => {
                    in_code_block = true;
                    code_block_lang = match lang {
                        pulldown_cmark::CodeBlockKind::Fenced(cow) => cow.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                    code_buffer.clear();
                    flush_line(&mut lines, &mut current_line);
                }
                Tag::List(_) => {}
                Tag::Item => {
                    current_line.push(Span::styled("  * ", Style::default().fg(theme.accent)));
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
                    current_line.push(Span::styled("  | ", Style::default().fg(theme.text_dim)));
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
                    flush_line(&mut lines, &mut current_line);
                }
                TagEnd::TableRow => {
                    if in_table {
                        flush_line(&mut lines, &mut current_line);
                    }
                }
                TagEnd::TableCell => {
                    if in_table {
                        current_line.push(Span::styled(" | ", Style::default().fg(theme.text_dim)));
                    }
                }
                TagEnd::Heading(_) => {
                    flush_line(&mut lines, &mut current_line);
                }
                TagEnd::CodeBlock => {
                    if !code_buffer.is_empty() {
                        let highlighted =
                            highlight_code(&code_buffer.join("\n"), &code_block_lang, theme);
                        lines.extend(highlighted);
                    }
                    in_code_block = false;
                    code_block_lang.clear();
                    code_buffer.clear();
                    flush_line(&mut lines, &mut current_line);
                }
                TagEnd::Item => {
                    flush_line(&mut lines, &mut current_line);
                }
                TagEnd::Paragraph => {
                    flush_line(&mut lines, &mut current_line);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buffer.push(text.to_string());
                } else {
                    let mut style = Style::default().fg(theme.fg);
                    for modifier in &style_stack {
                        style = style.add_modifier(*modifier);
                    }
                    current_line.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                current_line.push(Span::styled(
                    format!(" {code} "),
                    Style::default().fg(theme.warning).bg(theme.tool_bg),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut lines, &mut current_line);
            }
            Event::Rule => {
                flush_line(&mut lines, &mut current_line);
                let rule = "─".repeat(40);
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
        flush_line(&mut lines, &mut current_line);
    }

    lines
}

fn highlight_code(code: &str, lang: &str, theme: &Theme) -> Vec<Line<'static>> {
    if lang.is_empty() {
        return code
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.warning),
                ))
            })
            .collect();
    }

    let cache_key = (code.to_string(), lang.to_string(), theme.warning);
    if let Ok(cache) = HIGHLIGHT_CACHE.lock()
        && let Some(lines) = cache.get(&cache_key)
    {
        return lines.clone();
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
        return code
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.warning),
                ))
            })
            .collect();
    };
    let mut h = HighlightLines::new(syntax, syn_theme);

    let mut result = Vec::new();
    for line in code.lines() {
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled("  ", Style::default()));

        if let Ok(highlighted) = h.highlight_line(line, ss) {
            for (style, text) in highlighted {
                spans.push(Span::styled(text.to_string(), syntect_to_ratatui(style)));
            }
        } else {
            spans.push(Span::styled(
                line.to_string(),
                Style::default().fg(theme.warning),
            ));
        }

        result.push(Line::from(spans));
    }

    if code.len() <= HIGHLIGHT_CACHE_MAX_BYTES
        && let Ok(mut cache) = HIGHLIGHT_CACHE.lock()
    {
        if cache.len() >= HIGHLIGHT_CACHE_CAP {
            cache.clear();
        }
        cache.insert(cache_key, result.clone());
    }

    result
}

fn flush_line(lines: &mut Vec<Line<'static>>, current_line: &mut Vec<Span<'static>>) {
    if current_line.is_empty() {
        lines.push(Line::from(""));
    } else {
        let line: Line<'static> = Line::from(std::mem::take(current_line));
        lines.push(line);
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
}
