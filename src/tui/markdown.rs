use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::text::{Line, Span};
use ratatui::style::{Color, Modifier, Style};
use crate::tui::theme::Theme;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{ThemeSet, Style as SynStyle};
use syntect::parsing::SyntaxSet;

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(|| SyntaxSet::load_defaults_newlines())
}

fn theme_set() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(|| ThemeSet::load_defaults())
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

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    use pulldown_cmark::HeadingLevel;
                    let style = match level {
                        HeadingLevel::H1 => Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        HeadingLevel::H2 => Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                        _ => Style::default()
                            .fg(theme.fg)
                            .add_modifier(Modifier::BOLD),
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
                    current_line.push(Span::styled(
                        "  * ",
                        Style::default().fg(theme.accent),
                    ));
                }
                Tag::Emphasis => {
                    current_line.push(Span::styled("", Style::default().add_modifier(Modifier::ITALIC)));
                }
                Tag::Strong => {
                    current_line.push(Span::styled("", Style::default().add_modifier(Modifier::BOLD)));
                }
                Tag::Strikethrough => {
                    current_line.push(Span::styled("", Style::default().add_modifier(Modifier::CROSSED_OUT)));
                }
                Tag::BlockQuote(_) => {
                    current_line.push(Span::styled(
                        "  | ",
                        Style::default().fg(theme.text_dim),
                    ));
                }
                Tag::Link { dest_url, .. } => {
                    current_line.push(Span::styled(
                        format!("[{dest_url}]"),
                        Style::default().fg(theme.accent).add_modifier(Modifier::UNDERLINED),
                    ));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    flush_line(&mut lines, &mut current_line);
                }
                TagEnd::CodeBlock => {
                    if !code_buffer.is_empty() {
                        let highlighted = highlight_code(&code_buffer.join("\n"), &code_block_lang, theme);
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
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buffer.push(text.to_string());
                } else {
                    current_line.push(Span::styled(text.to_string(), Style::default().fg(theme.fg)));
                }
            }
            Event::Code(code) => {
                current_line.push(Span::styled(
                    format!(" {code} "),
                    Style::default().fg(theme.warning).bg(theme.tool_bg),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    flush_line(&mut lines, &mut current_line);
                } else {
                    flush_line(&mut lines, &mut current_line);
                }
            }
            Event::Rule => {
                flush_line(&mut lines, &mut current_line);
                let rule = "─".repeat(40);
                lines.push(Line::from(vec![
                    Span::styled(rule, Style::default().fg(theme.text_dim)),
                ]));
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
        return code.lines().map(|line| {
            Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.warning),
            ))
        }).collect();
    }

    let ss = syntax_set();
    let ts = theme_set();

    let syntax = ss.find_syntax_by_token(lang)
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
                spans.push(Span::styled(
                    text.to_string(),
                    syntect_to_ratatui(style),
                ));
            }
        } else {
            spans.push(Span::styled(
                line.to_string(),
                Style::default().fg(theme.warning),
            ));
        }

        result.push(Line::from(spans));
    }

    result
}

fn flush_line(lines: &mut Vec<Line<'static>>, current_line: &mut Vec<Span<'static>>) {
    if current_line.is_empty() {
        lines.push(Line::from(""));
    } else {
        let line: Line<'static> = Line::from(current_line.drain(..).collect::<Vec<_>>());
        lines.push(line);
    }
}
