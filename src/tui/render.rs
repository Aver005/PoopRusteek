use crate::app::AppState;
use crate::app::events::Modal;
use crate::config::Config;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::cell::Cell;

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    let cursor_cell = Cell::new(None);
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(String::new()).style(Style::default().bg(theme.bg)),
            area,
        );

        if state.messages.is_empty() && !state.is_generating {
            render_landing(frame, area, state, config, &theme, &cursor_cell);
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(4),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(area);

            render_separator(frame, chunks[1], &theme);
            widgets::chat::render_chat(frame, chunks[0], state, &theme);
            widgets::input::render_input(frame, chunks[2], state, &theme, false, &cursor_cell);
            render_input_border(frame, chunks[3], &theme);
            render_mini_status(frame, chunks[4], state, config, &theme);

            if state.autocomplete.visible {
                render_autocomplete(frame, chunks[2], state, &theme);
            }
        }

        if let Some(modal) = &state.modal {
            render_modal(frame, area, modal, &theme);
        }
    })?;
    // Re-set cursor position after frame flush to prevent flicker onto (0,0)
    if let Some((cx, cy)) = cursor_cell.get() {
        use crossterm::cursor::MoveTo;
        crossterm::execute!(terminal.backend_mut(), MoveTo(cx, cy))?;
    }
    if state.modal.is_some() || state.is_generating {
        terminal.hide_cursor()?;
    } else {
        terminal.show_cursor()?;
    }
    Ok(())
}

fn render_separator(frame: &mut Frame, area: Rect, theme: &Theme) {
    let line = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            line,
            Style::default().fg(theme.border).bg(theme.bg),
        ))),
        area,
    );
}

fn render_input_border(frame: &mut Frame, area: Rect, theme: &Theme) {
    let w = area.width.saturating_sub(3) as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}{}", "   ", "─".repeat(w)),
            Style::default().fg(theme.border).bg(theme.input_bg),
        ))),
        area,
    );
}

fn render_landing(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme, cursor_cell: &Cell<Option<(u16, u16)>>) {
    let input_width = area.width.min(76);
    let x = (area.width - input_width) / 2;

    let sessions = crate::session::list_sessions(config)
        .unwrap_or_default();
    let session_count = sessions.len();
    let show_sessions = session_count > 0;

    // Logo(3) + gap(1) + input(4) + border(1) + shortcuts(1) + gap(1) + sessions(if any: 1+count.min(5)+1) + gap + status(1)
    let center_h = 3 + 1 + 4 + 1 + 1 + 1
        + if show_sessions { 1 + sessions.len().min(5) as u16 + 1 } else { 0 };
    let top_pad = (area.height.saturating_sub(center_h)) / 2;

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(top_pad),
        Constraint::Length(3),   // logo block
        Constraint::Length(1),   // gap
        Constraint::Length(4),   // input
        Constraint::Length(1),   // border
        Constraint::Length(1),   // shortcuts
        Constraint::Length(1),   // gap
    ];
    if show_sessions {
        let count = sessions.len().min(5) as u16;
        constraints.push(Constraint::Length(count + 2)); // header + rows + blank
    }
    constraints.push(Constraint::Min(0)); // bottom space

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut ci = 1;

    // Logo block
    let logo_area = centered_h(chunks[ci], input_width);
    ci += 1;
    let title_spans = bigger_title(state, theme);
    frame.render_widget(
        Paragraph::new(Line::from(title_spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(theme.bg)),
        logo_area,
    );

    // skip gap (ci points to gap, which we skip by incrementing)
    ci += 1;

    // Input — 4 lines, centered
    let inp_area = Rect::new(x, chunks[ci].y, input_width, 4);
    ci += 1;
    widgets::input::render_input(frame, inp_area, state, theme, true, cursor_cell);

    if state.autocomplete.visible {
        render_autocomplete(frame, inp_area, state, theme);
    }

    // Border below input
    let border_area = centered_h(chunks[ci], input_width);
    ci += 1;
    render_input_border(frame, border_area, &theme);

    // Shortcuts line
    let info_area = centered_h(chunks[ci], input_width);
    ci += 1;
    let model = format!("{} · {}", provider_label(config), config.provider.model);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(model, Style::default().fg(theme.text_soft).bg(theme.bg)),
                Span::styled("  ", Style::default().bg(theme.bg)),
                Span::styled("Enter ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
                Span::styled("send  ", Style::default().fg(theme.text_dim)),
                Span::styled("/ ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
                Span::styled("commands  ", Style::default().fg(theme.text_dim)),
                Span::styled("Ctrl+C ", Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)),
                Span::styled("quit", Style::default().fg(theme.text_dim)),
            ]),
        ])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg)),
        info_area,
    );

    // skip gap
    ci += 1;

    // Sessions table
    if show_sessions {
        let sc = chunks[ci];
        ci += 1;
        render_sessions_table(frame, sc, &sessions, input_width, theme);
    }

    // Status at very bottom
    let status_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    render_mini_status(frame, status_area, state, config, theme);
}

fn render_sessions_table(
    frame: &mut Frame,
    area: Rect,
    sessions: &[crate::session::SessionSummary],
    input_width: u16,
    theme: &Theme,
) {
    let count = sessions.len().min(5);
    let area = centered_h(area, input_width);
    let header_style = Style::default().fg(theme.text_dim).bg(theme.bg);
    let id_style = Style::default().fg(theme.accent_soft).bg(theme.bg);
    let title_style = Style::default().fg(theme.fg).bg(theme.bg);
    let date_style = Style::default().fg(theme.text_dim).bg(theme.bg);
    let sep_style = Style::default().fg(theme.border).bg(theme.bg);

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled(" Recent sessions ", header_style),
    ]));
    lines.push(Line::from(vec![Span::styled(
        format!("  {} {}  {}", "──".repeat(8), "──".repeat(15), "──".repeat(8)),
        sep_style,
    )]));

    for s in sessions.iter().take(5) {
        let id_short = truncate(&s.id, 16);
        let title = truncate(&s.title, 30);
        let date = format_date(&s.updated_at);
        lines.push(Line::from(vec![
            Span::styled(format!("  {id_short}"), id_style),
            Span::styled("  ", Style::default().bg(theme.bg)),
            Span::styled(format!("{title}"), title_style),
            Span::styled("  ", Style::default().bg(theme.bg)),
            Span::styled(date, date_style),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

fn format_date(rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
        dt.format("%b %d %H:%M").to_string()
    } else {
        rfc3339.chars().take(16).collect()
    }
}

fn centered_h(area: Rect, width: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    Rect::new(x, area.y, width.min(area.width), area.height)
}

fn render_mini_status(frame: &mut Frame, area: Rect, state: &AppState, config: &Config, theme: &Theme) {
    let provider_name = match config.provider.kind {
        crate::config::ProviderKind::Deepseek => "DeepSeek",
        crate::config::ProviderKind::Openai => "OpenAI",
        crate::config::ProviderKind::Custom => "Custom",
    };
    let model = &config.provider.model;
    let msg_count = state.messages.len();
    let spinner = match state.animation_tick % 4 {
        0 => "|",
        1 => "/",
        2 => "-",
        _ => "\\",
    };

    let left = format!(" {} · {} ", provider_name, model);
    let center = if state.is_generating {
        format!(" {} {} ", spinner, state.status_message)
    } else {
        format!(" {} ", state.status_message)
    };
    let right = format!(" {} msgs", msg_count);

    let gap = area.width.saturating_sub((left.len() + center.len() + right.len()) as u16).max(1) as usize;

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(theme.text_dim).bg(theme.bg)),
            Span::styled(" ".repeat(gap), Style::default().bg(theme.bg)),
            Span::styled(
                center,
                if state.is_generating {
                    Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                }
                .bg(theme.bg),
            ),
            Span::styled(right, Style::default().fg(theme.text_dim).bg(theme.bg)),
        ])),
        area,
    );
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &Modal, theme: &Theme) {
    match modal {
        Modal::ToolApproval { tool_name, arguments, scroll_offset, always_allow } => {
            let popup_width = area.width.min(72).max(50);
            let max_height = area.height.saturating_sub(4);
            let content_height = arguments.lines().count().max(4).min(max_height.saturating_sub(6) as usize);
            let popup_height = (content_height + 8).min(max_height as usize) as u16;

            let x = (area.width.saturating_sub(popup_width)) / 2;
            let y = (area.height.saturating_sub(popup_height)) / 2;
            let popup_area = Rect::new(x, y, popup_width, popup_height);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .title(" \u{26A0} Tool Call ")
                .title_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.panel));

            let inner = block.inner(popup_area);
            let inner_w = inner.width as usize;
            let inner_h = inner.height as usize;

            let mut all_lines: Vec<Line> = Vec::new();

            // Tool name line
            all_lines.push(Line::from(vec![
                Span::styled("  \u{2692} ", Style::default().fg(theme.warning)),
                Span::styled("Tool:", Style::default().fg(theme.text_dim)),
                Span::styled(" ", Style::default().fg(theme.fg)),
                Span::styled(
                    tool_name.clone(),
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
            ]));
            all_lines.push(Line::from(""));

            // Arguments header
            all_lines.push(Line::from(vec![
                Span::styled("  \u{1F4CB} Arguments:", Style::default().fg(theme.text_dim)),
            ]));

            // Parse and format arguments with JSON highlighting
            let arg_lines = highlight_json(arguments, theme);
            let max_visible = inner_h.saturating_sub(6);
            let total_args = arg_lines.len();
            let scroll = *scroll_offset;
            let display_lines: Vec<&Line> = if total_args > max_visible {
                let end = (scroll + max_visible).min(total_args);
                arg_lines[scroll..end].iter().collect()
            } else {
                arg_lines.iter().collect()
            };

            for line in &display_lines {
                let mut line_spans = vec![Span::styled("  ", Style::default().fg(theme.fg))];
                for span in &line.spans {
                    line_spans.push(Span::styled(
                        span.content.clone(),
                        span.style,
                    ));
                }
                all_lines.push(Line::from(line_spans));
            }

            // Scroll indicator
            if total_args > max_visible {
                let pct = if total_args > 0 {
                    (scroll as f64 / (total_args.saturating_sub(max_visible)) as f64 * 100.0) as u16
                } else {
                    0
                };
                let indicator = format!("\u{2191}\u{2193} {:.0}%", pct.min(100));
                all_lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", indicator),
                        Style::default().fg(theme.text_dim),
                    ),
                ]));
            }

            // Fill remaining space
            while all_lines.len() < inner_h.saturating_sub(3) {
                all_lines.push(Line::from(""));
            }

            // Separator
            let sep = "\u{2500}".repeat(inner_w.saturating_sub(4));
            all_lines.push(Line::from(vec![
                Span::styled(format!("  {}", sep), Style::default().fg(theme.border)),
            ]));

            // Always allow checkbox
            let check = if *always_allow { "\u{2611}" } else { "\u{2610}" };
            all_lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} Always allow for this session", check),
                    if *always_allow {
                        Style::default().fg(theme.success)
                    } else {
                        Style::default().fg(theme.text_dim)
                    },
                ),
            ]));

            // Key bindings
            all_lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.fg)),
                Span::styled("Y", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
                Span::styled(" allow  ", Style::default().fg(theme.text_dim)),
                Span::styled("N", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
                Span::styled(" deny  ", Style::default().fg(theme.text_dim)),
                Span::styled("A", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                Span::styled(" toggle  ", Style::default().fg(theme.text_dim)),
                Span::styled("\u{2191}\u{2193}", Style::default().fg(theme.accent_soft)),
                Span::styled(" scroll", Style::default().fg(theme.text_dim)),
            ]));

            frame.render_widget(Clear, popup_area);
            frame.render_widget(block, popup_area);
            frame.render_widget(
                Paragraph::new(all_lines).style(Style::default().bg(theme.panel)),
                inner,
            );
        }
        Modal::Confirm { message, .. } => {
            let pw = area.width.min(60);
            let ph = 6u16.min(area.height.saturating_sub(2));
            let x = (area.width.saturating_sub(pw)) / 2;
            let y = (area.height.saturating_sub(ph)) / 2;
            let popup_area = Rect::new(x, y, pw, ph);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .title(" Confirm ")
                .title_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.bg));

            let lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("  {message}"),
                    Style::default().fg(theme.fg),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  y", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
                    Span::styled(" = yes  ", Style::default().fg(theme.text_dim)),
                    Span::styled("n", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
                    Span::styled(" = no", Style::default().fg(theme.text_dim)),
                ]),
            ];

            frame.render_widget(Clear, popup_area);
            let inner = block.inner(popup_area);
            frame.render_widget(block, popup_area);
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).style(Style::default().bg(theme.bg)),
                inner,
            );
        }
        Modal::Input { prompt } => {
            let pw = area.width.min(60);
            let ph = 6u16.min(area.height.saturating_sub(2));
            let x = (area.width.saturating_sub(pw)) / 2;
            let y = (area.height.saturating_sub(ph)) / 2;
            let popup_area = Rect::new(x, y, pw, ph);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(" Input ")
                .title_style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))
                .style(Style::default().bg(theme.bg));

            let lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("  {prompt}"),
                    Style::default().fg(theme.fg),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        "  Press Esc to cancel",
                        Style::default().fg(theme.text_dim),
                    ),
                ]),
            ];

            frame.render_widget(Clear, popup_area);
            let inner = block.inner(popup_area);
            frame.render_widget(block, popup_area);
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).style(Style::default().bg(theme.bg)),
                inner,
            );
        }
    }
}

fn highlight_json(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    fn value_style(v: &str, theme: &Theme) -> Style {
        let t = v.trim();
        if t.starts_with('"') && t.ends_with('"') {
            Style::default().fg(theme.success)
        } else if t == "true" || t == "false" {
            Style::default().fg(theme.accent_soft)
        } else if t == "null" {
            Style::default().fg(theme.text_dim)
        } else if t.parse::<f64>().is_ok() {
            Style::default().fg(Color::Rgb(255, 203, 107))
        } else {
            Style::default().fg(theme.fg)
        }
    }

    let result: Vec<Line<'static>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim();
            let indent_len = line.len().saturating_sub(trimmed.len());
            let indent = &line[..indent_len];

            if let Some((k, rest)) = trimmed.split_once(':') {
                let key_part = k.trim();
                let after_colon = rest.trim();
                let has_comma = after_colon.ends_with(',');
                let val_str = after_colon.trim_end_matches(',');

                let mut spans = vec![Span::styled(indent.to_string(), Style::default().fg(theme.fg))];
                spans.push(Span::styled(
                    key_part.to_string(),
                    Style::default().fg(theme.accent),
                ));
                spans.push(Span::styled(
                    ": ".to_string(),
                    Style::default().fg(theme.text_dim),
                ));

                if val_str.starts_with('{') || val_str.starts_with('[') {
                    spans.push(Span::styled(
                        val_str.to_string(),
                        Style::default().fg(theme.fg),
                    ));
                } else {
                    spans.push(Span::styled(
                        val_str.to_string(),
                        value_style(val_str, theme),
                    ));
                }
                if has_comma {
                    spans.push(Span::styled(
                        ",".to_string(),
                        Style::default().fg(theme.text_dim),
                    ));
                }
                Line::from(spans)
            } else {
                let t = trimmed;
                let style = if t == "{" || t == "}" || t == "[" || t == "]" {
                    Style::default().fg(theme.text_dim)
                } else if t.starts_with('"') {
                    value_style(t, theme)
                } else {
                    Style::default().fg(theme.fg)
                };
                Line::from(vec![
                    Span::styled(indent.to_string(), Style::default().fg(theme.fg)),
                    Span::styled(t.to_string(), style),
                ])
            }
        })
        .collect();

    result
}

fn provider_label(config: &Config) -> &'static str {
    match config.provider.kind {
        crate::config::ProviderKind::Deepseek => "DeepSeek",
        crate::config::ProviderKind::Openai => "OpenAI",
        crate::config::ProviderKind::Custom => "Custom",
    }
}

const AUTOCOMPLETE_VISIBLE: usize = 8;

fn render_autocomplete(frame: &mut Frame, input_area: Rect, state: &AppState, theme: &Theme) {
    let items = &state.autocomplete.items;
    if items.is_empty() {
        return;
    }
    let total = items.len();

    let visible_count = total.min(AUTOCOMPLETE_VISIBLE);
    let scroll_off = state.autocomplete.scroll_offset.min(total.saturating_sub(1));
    let view_end = (scroll_off + visible_count).min(total);
    let vis_items = &items[scroll_off..view_end];

    let max_name_width = items
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(6) as u16;
    let popup_h = (visible_count + 2) as u16;
    let popup_w = input_area
        .width
        .min(64)
        .max(max_name_width + 28)
        .min(input_area.width);

    let max_y_above = input_area.y.saturating_sub(1);
    let popup_h = popup_h.min(max_y_above.max(2));
    let popup_y = input_area.y.saturating_sub(popup_h);
    let popup_area = Rect::new(input_area.x, popup_y, popup_w, popup_h);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(" Commands ")
        .title_style(
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let sel_style = Style::default()
        .fg(theme.bg)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let name_style = Style::default().fg(theme.accent_soft).bg(theme.panel);
    let desc_style = Style::default().fg(theme.text_soft).bg(theme.panel);
    let usage_style = Style::default().fg(theme.text_dim).bg(theme.panel);
    let muted = Style::default().fg(theme.text_dim).bg(theme.panel);

    let max_name_w = max_name_width as usize;

    let lines: Vec<Line> = vis_items
        .iter()
        .enumerate()
        .map(|(rel, item)| {
            let is_sel = (scroll_off + rel) == state.autocomplete.selected;
            let indicator = if is_sel { "▸ " } else { "  " };
            let slash = "/";
            let name_full = format!("{slash}{}", item.name);
            let padding = max_name_w
                .saturating_sub(item.name.chars().count())
                .max(1);
            let pad_str = " ".repeat(padding);

            let mut spans: Vec<Span> = Vec::new();
            if is_sel {
                spans.push(Span::styled(indicator, sel_style));
                spans.push(Span::styled(name_full, sel_style));
                spans.push(Span::styled(pad_str, sel_style));
                let desc_text = if item.description.is_empty() {
                    String::new()
                } else {
                    item.description.clone()
                };
                spans.push(Span::styled(desc_text, sel_style));
                if !item.usage.is_empty() {
                    spans.push(Span::styled("  ", sel_style));
                    spans.push(Span::styled(item.usage.clone(), sel_style));
                }
            } else {
                spans.push(Span::styled(indicator, muted));
                spans.push(Span::styled(name_full, name_style));
                spans.push(Span::styled(pad_str, muted));
                spans.push(Span::styled("  ", muted));
                spans.push(Span::styled(item.description.clone(), desc_style));
                if !item.usage.is_empty() {
                    spans.push(Span::styled("  ", muted));
                    spans.push(Span::styled(item.usage.clone(), usage_style));
                }
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.panel)),
        inner,
    );
}

fn bigger_title(state: &AppState, theme: &Theme) -> Vec<Span<'static>> {
    let text = "POOPRUSTEEK";
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let pulse = ((state.animation_tick as usize / 5) + index) % 6;
            let color = match pulse {
                0 | 1 => theme.accent_soft,
                2 | 3 => theme.accent,
                _ => theme.success,
            };
            let bright = if index == 0 || index == 5 || index == 9 {
                Modifier::BOLD | Modifier::UNDERLINED
            } else {
                Modifier::BOLD
            };
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(color)
                    .bg(theme.bg)
                    .add_modifier(bright),
            )
        })
        .collect()
}
