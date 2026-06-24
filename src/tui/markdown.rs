use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::text::{Line, Span};
use ratatui::style::{Modifier, Style};
use crate::tui::theme::Theme;

pub fn render_markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(text, options);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;

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
                    let lang_str = match lang {
                        pulldown_cmark::CodeBlockKind::Fenced(cow) => cow.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                    if !lang_str.is_empty() {
                        flush_line(&mut lines, &mut current_line);
                        current_line.push(Span::styled(
                            format!("  [{lang_str}]"),
                            Style::default().fg(theme.accent_dim),
                        ));
                    }
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
                    in_code_block = false;
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
                let style = if in_code_block {
                    Style::default().fg(theme.warning)
                } else {
                    Style::default().fg(theme.fg)
                };
                current_line.push(Span::styled(text.to_string(), style));
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

fn flush_line(lines: &mut Vec<Line<'static>>, current_line: &mut Vec<Span<'static>>) {
    if current_line.is_empty() {
        lines.push(Line::from(""));
    } else {
        let line: Line<'static> = Line::from(current_line.drain(..).collect::<Vec<_>>());
        lines.push(line);
    }
}
