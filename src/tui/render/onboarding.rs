//! The full-screen first-run onboarding view (`View::Onboarding`) and the
//! animated logo it shares with the landing screen.

use crate::app::events::OnboardingState;
use crate::tui::theme::Theme;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::cell::Cell;

pub(super) fn render_onboarding(
    frame: &mut Frame,
    area: Rect,
    ob: &OnboardingState,
    animation_tick: u64,
    theme: &Theme,
    cursor_cell: &Cell<Option<(u16, u16)>>,
) {
    // ── Layout constants ───────────────────────────────────────────
    const MAX_W: u16 = 72;
    // logo(1) + gap(1) + tagline(1) + gap(1) + steps_panel(7) + gap(1)
    // + input_box(3) + model_line(1) + gap(1) + footer(1) + error_reserve(1)
    const CONTENT_H: u16 = 19;

    let col_w = area.width.min(MAX_W);
    let col_x = area.x + area.width.saturating_sub(col_w) / 2;
    let top_pad = area.height.saturating_sub(CONTENT_H) / 2;

    // Build a vertical layout within the centered column.
    let col_area = Rect::new(col_x, area.y, col_w, area.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),   // 0 — top padding
            Constraint::Length(1),          // 1 — logo
            Constraint::Length(1),          // 2 — gap
            Constraint::Length(1),          // 3 — tagline
            Constraint::Length(1),          // 4 — gap
            Constraint::Length(7),          // 5 — steps panel
            Constraint::Length(1),          // 6 — gap
            Constraint::Length(3),          // 7 — token input box
            Constraint::Length(1),          // 8 — model selector
            Constraint::Length(1),          // 9 — gap
            Constraint::Length(1),          // 10 — footer
            Constraint::Length(1),          // 11 — error line
            Constraint::Min(0),             // 12 — remaining
        ])
        .split(col_area);

    // ── 1. Logo ────────────────────────────────────────────────────
    let title_spans = pulsing_title(animation_tick, theme);
    frame.render_widget(
        Paragraph::new(Line::from(title_spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(theme.bg)),
        chunks[1],
    );

    // ── 3. Tagline ─────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Terminal coding agent · powered by DeepSeek web",
            Style::default().fg(theme.text_dim).bg(theme.bg),
        )))
        .alignment(Alignment::Center),
        chunks[3],
    );

    // ── 5. Steps panel ────────────────────────────────────────────
    // 7 rows total: border-top(1) + 3 step lines(3) + 2 blank rows(2) + border-bottom(1).
    // We use a Block with a title and render paragraphs inside.
    let steps_block = Block::default()
        .title(" Getting started ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.bg))
        .style(Style::default().bg(theme.bg));
    let steps_inner = steps_block.inner(chunks[5]);
    frame.render_widget(steps_block, chunks[5]);

    let steps = [
        ("1.", "Log in at chat.deepseek.com in your browser"),
        ("2.", "DevTools (F12) → Application → Local Storage → copy userToken"),
        ("3.", "Paste it below and press Enter"),
    ];
    let step_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(steps_inner);
    for (i, (num, text)) in steps.iter().enumerate() {
        let line = Line::from(vec![
            Span::styled(
                format!("{num} "),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
            ),
            Span::styled(*text, Style::default().fg(theme.text_soft).bg(theme.bg)),
        ]);
        frame.render_widget(Paragraph::new(line), step_chunks[i]);
    }

    // ── 7. Token input box ────────────────────────────────────────
    let input_block = Block::default()
        .title(" DeepSeek token ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus).bg(theme.input_bg))
        .style(Style::default().bg(theme.input_bg));
    let input_inner = input_block.inner(chunks[7]);
    frame.render_widget(input_block, chunks[7]);

    // Clip the visible portion so the cursor stays in view: show the tail end.
    let box_w = input_inner.width as usize;
    let input_chars: Vec<char> = ob.input.chars().collect();
    let total_chars = input_chars.len();
    // cursor is clamped to [0, total_chars]
    let cursor = ob.cursor.min(total_chars);
    // How many chars fit before the cursor (clipped from the left).
    let visible_start = if cursor >= box_w { cursor - (box_w - 1) } else { 0 };
    let visible_chars: String = input_chars[visible_start..total_chars].iter().collect();
    // Truncate to box width using char-aware slicing (never byte-slice — invariant).
    let display: String = visible_chars.chars().take(box_w).collect();
    let cursor_col = (cursor - visible_start) as u16;

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            display,
            Style::default().fg(theme.fg).bg(theme.input_bg),
        )))
        .style(Style::default().bg(theme.input_bg)),
        input_inner,
    );
    // Place block cursor at the right terminal cell.
    cursor_cell.set(Some((input_inner.x + cursor_col, input_inner.y)));

    // ── 8. Model selector ────────────────────────────────────────
    let (chat_style, reasoner_style, chat_dot, reasoner_dot) = if ob.model_reasoner {
        (
            Style::default().fg(theme.text_dim).bg(theme.bg),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
            "○",
            "●",
        )
    } else {
        (
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD).bg(theme.bg),
            Style::default().fg(theme.text_dim).bg(theme.bg),
            "●",
            "○",
        )
    };
    let model_line = Line::from(vec![
        Span::styled("Model:  ", Style::default().fg(theme.text_dim).bg(theme.bg)),
        Span::styled(format!("{chat_dot} deepseek-chat"), chat_style),
        Span::styled("   ", Style::default().bg(theme.bg)),
        Span::styled(format!("{reasoner_dot} deepseek-reasoner"), reasoner_style),
        Span::styled("   (Tab to switch)", Style::default().fg(theme.text_dim).bg(theme.bg)),
    ]);
    frame.render_widget(
        Paragraph::new(model_line).alignment(Alignment::Center),
        chunks[8],
    );

    // ── 10. Footer hints ──────────────────────────────────────────
    let footer = Line::from(vec![
        Span::styled(
            "Enter",
            Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD).bg(theme.bg),
        ),
        Span::styled(" — save & start    ", Style::default().fg(theme.text_dim).bg(theme.bg)),
        Span::styled(
            "Ctrl+C",
            Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD).bg(theme.bg),
        ),
        Span::styled(" — quit", Style::default().fg(theme.text_dim).bg(theme.bg)),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        chunks[10],
    );

    // ── 11. Error line ────────────────────────────────────────────
    if let Some(msg) = ob.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.error).bg(theme.bg),
            )))
            .alignment(Alignment::Center),
            chunks[11],
        );
    }
}

/// Animated pulsing logo shared by the landing and onboarding screens.
pub(super) fn pulsing_title(animation_tick: u64, theme: &Theme) -> Vec<Span<'static>> {
    let text = "POOPRUSTEEK";
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let pulse = ((animation_tick as usize / 5) + index) % 6;
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
                Style::default().fg(color).bg(theme.bg).add_modifier(bright),
            )
        })
        .collect()
}
