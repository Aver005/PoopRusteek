//! The `/themes` screen: gallery of presets + custom themes on the left, a
//! live preview pane on the right. The *entire frame* is already drawn with
//! the highlighted (or wizard's in-progress) theme — `render()` resolves it
//! via `ThemesViewState::preview_theme` — so this module's preview pane only
//! showcases the roles that don't appear in the surrounding chrome (message
//! backgrounds, status colors, selection, the full swatch board).

use super::chrome::{panel_footer, panel_frame};
use super::popup::{option_row, push_text_box_lines};
use crate::app::list::{ListWindow, NAV_HINT, rows as list_rows};
use crate::app::themes::{
    ThemeRowKind, ThemeWizardState, ThemeWizardStep, ThemesViewState, theme_rows,
};
use crate::config::Config;
use crate::tui::theme::{PRESETS, ROLES, Theme};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub(super) fn render_themes_view(
    frame: &mut Frame,
    area: Rect,
    view: &ThemesViewState,
    config: &Config,
    theme: &Theme,
) {
    let subtitle = if view.wizard.is_some() {
        "Create your own theme — every keystroke restyles the whole app live"
    } else {
        "Pick a look — moving the cursor previews it live, Enter makes it stick"
    };
    let (body_area, footer_area) = panel_frame(frame, area, " Themes ", subtitle, theme);

    // Body: gallery / wizard on the left, preview pane on the right.
    let left_w = 46u16.min(body_area.width / 2).max(24);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_w), Constraint::Min(20)])
        .split(body_area);

    match &view.wizard {
        Some(wizard) => render_wizard_panel(frame, body[0], wizard, theme),
        None => render_theme_list(frame, body[0], view, config, theme),
    }
    render_preview_panel(frame, body[1], view, theme);

    let hint = match &view.wizard {
        None => format!("  {NAV_HINT} preview  Enter apply  n new  e edit  d delete  Esc/q back  "),
        Some(wizard) => match wizard.step {
            ThemeWizardStep::Base => "  ↑↓ choose palette  Enter next  Esc back  ".to_string(),
            ThemeWizardStep::Color(_) => {
                "  Enter keep & next  Ctrl+S finish now  Esc previous  ".to_string()
            }
            ThemeWizardStep::Confirm => "  Enter save & apply  Esc back  ".to_string(),
            ThemeWizardStep::Name => "  Enter next  Esc cancel  ".to_string(),
        },
    };
    panel_footer(frame, footer_area, hint, theme);
}

fn render_theme_list(
    frame: &mut Frame,
    area: Rect,
    view: &ThemesViewState,
    config: &Config,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Gallery ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = theme_rows(config);
    // Две строки на тему: название и описание.
    let capacity = inner.height as usize / list_rows::THEME as usize;
    let window = ListWindow::anchored(view.selected, rows.len(), capacity);

    let mut lines: Vec<Line> = Vec::new();
    for i in window.range() {
        let row = &rows[i];
        let selected = i == view.selected;
        let bg = if selected { theme.selection } else { theme.bg };
        let marker = if row.active { " ● " } else { "   " };
        let label_style = match row.kind {
            _ if selected => Style::default()
                .fg(theme.accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
            ThemeRowKind::Create => Style::default().fg(theme.accent_soft),
            _ => Style::default().fg(theme.fg),
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.success).bg(bg)),
            Span::styled(
                if selected { "▶ " } else { "  " },
                Style::default().fg(theme.accent).bg(bg),
            ),
            Span::styled(row.label.clone(), label_style),
        ]));
        lines.push(Line::from(Span::styled(
            format!("      {}", row.detail),
            Style::default().fg(theme.text_dim),
        )));
    }

    if !view.status_message.is_empty() && inner.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                view.status_message.clone(),
                Style::default().fg(theme.success),
            ))),
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
        );
    }

    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        ),
    );
}

fn render_wizard_panel(frame: &mut Frame, area: Rect, wizard: &ThemeWizardState, theme: &Theme) {
    let title = if wizard.editing.is_some() {
        " Edit theme "
    } else {
        " New theme "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.border_focus));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!(
                " Step {}/{} — ",
                wizard.step.number(),
                ThemeWizardStep::total()
            ),
            Style::default().fg(theme.text_dim),
        ),
        Span::styled(
            wizard.step.label(),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    match wizard.step {
        ThemeWizardStep::Name => {
            lines.push(Line::from(Span::styled(
                "  A name for your theme (also its /themes <name> handle):",
                Style::default().fg(theme.text_soft),
            )));
            lines.push(Line::from(""));
            push_text_box_lines(
                &mut lines,
                &wizard.name.buffer,
                wizard.name.cursor,
                theme,
                1,
            );
        }
        ThemeWizardStep::Base => {
            lines.push(Line::from(Span::styled(
                "  Start from a palette, then tweak any role:",
                Style::default().fg(theme.text_soft),
            )));
            lines.push(Line::from(""));
            for (i, preset) in PRESETS.iter().enumerate() {
                lines.push(option_row(preset.label, i == wizard.base_selected, theme));
            }
        }
        ThemeWizardStep::Color(index) => {
            let preview = wizard.preview_theme();
            let current = ROLES
                .get(index)
                .and_then(|role| preview.get(role.key))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled("  Current: ", Style::default().fg(theme.text_soft)),
                Span::styled("██████", Style::default().fg(current)),
            ]));
            lines.push(Line::from(Span::styled(
                "  Type a #rrggbb value (Enter keeps what's shown):",
                Style::default().fg(theme.text_dim),
            )));
            lines.push(Line::from(""));
            push_text_box_lines(
                &mut lines,
                &wizard.hex_input.buffer,
                wizard.hex_input.cursor,
                theme,
                1,
            );
        }
        ThemeWizardStep::Confirm => {
            lines.push(Line::from(Span::styled(
                format!("  Name:  {}", wizard.name.buffer.trim()),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Base:  {}", wizard.base_preset().label),
                Style::default().fg(theme.fg),
            )));
            lines.push(Line::from(Span::styled(
                format!("  Roles: {} colors set", wizard.colors.len()),
                Style::default().fg(theme.text_soft),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Enter saves the theme and applies it immediately.",
                Style::default().fg(theme.text_dim),
            )));
        }
    }

    if let Some(error) = &wizard.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  ⚠ {error}"),
            Style::default().fg(theme.error),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The showcase pane: a mock chat exchange plus the full role swatch board,
/// all drawn with the (already frame-wide) preview theme. In the wizard's
/// color steps the role being edited is marked on the board.
fn render_preview_panel(frame: &mut Frame, area: Rect, view: &ThemesViewState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Preview ")
        .border_style(Style::default().fg(theme.border_focus));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let editing_role = match &view.wizard {
        Some(wizard) => match wizard.step {
            ThemeWizardStep::Color(index) => Some(index),
            _ => None,
        },
        None => None,
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "  PoopRusteek \u{1F9FB}",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat((inner.width as usize).saturating_sub(4))),
        Style::default().fg(theme.border),
    )));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            " you ▸ make the terminal beautiful ",
            Style::default().fg(theme.fg).bg(theme.user_bg),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "  Sure — here's a live preview of every color role.",
        Style::default().fg(theme.fg),
    )));
    lines.push(Line::from(Span::styled(
        "  12:45 · deepseek-chat · 1.2k tokens",
        Style::default().fg(theme.text_dim),
    )));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            " ⚙ shell ",
            Style::default().fg(theme.accent_soft).bg(theme.tool_bg),
        ),
        Span::styled(
            " cargo test — 227 passed ",
            Style::default().fg(theme.text_soft).bg(theme.tool_bg),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  ✔ success  ", Style::default().fg(theme.success)),
        Span::styled("⚠ warning  ", Style::default().fg(theme.warning)),
        Span::styled("✖ error  ", Style::default().fg(theme.error)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            " selected text ",
            Style::default().fg(theme.fg).bg(theme.selection),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(" ❯ input", Style::default().fg(theme.fg).bg(theme.input_bg)),
        Span::styled("█", Style::default().fg(theme.accent).bg(theme.input_bg)),
        Span::styled("        ", Style::default().bg(theme.input_bg)),
    ]));
    lines.push(Line::from(""));

    // Swatch board: two columns of "██ role", the edited role highlighted.
    let mid = ROLES.len().div_ceil(2);
    for row in 0..mid {
        let mut spans: Vec<Span> = vec![Span::styled("  ", Style::default())];
        for column in [row, row + mid] {
            let Some(role) = ROLES.get(column) else {
                continue;
            };
            let color = theme.get(role.key).unwrap_or_default();
            let is_editing = editing_role == Some(column);
            spans.push(Span::styled(
                if is_editing { "▶" } else { " " },
                Style::default().fg(theme.accent),
            ));
            spans.push(Span::styled("██ ", Style::default().fg(color)));
            spans.push(Span::styled(
                format!("{:<14}", role.key),
                if is_editing {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                },
            ));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}
