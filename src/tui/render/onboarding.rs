//! The full-screen first-run onboarding view (`View::Onboarding`) and the
//! animated logo it shares with the landing screen.

use crate::app::events::{OnboardingState, TOKEN_CONSOLE_SNIPPET};
use crate::tui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
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
    // logo(1) + gap(1) + tagline(1) + gap(1) + steps_panel(STEPS_H) + gap(1)
    // + input_box(3) + model_line(1) + gap(1) + footer(1) + status_line(1)
    const CONTENT_H: u16 = 12 + STEPS_H;

    let col_w = area.width.min(MAX_W);
    let col_x = area.x + area.width.saturating_sub(col_w) / 2;
    let top_pad = area.height.saturating_sub(CONTENT_H) / 2;

    // Build a vertical layout within the centered column.
    let col_area = Rect::new(col_x, area.y, col_w, area.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad), // 0 — top padding
            Constraint::Length(1),       // 1 — logo
            Constraint::Length(1),       // 2 — gap
            Constraint::Length(1),       // 3 — tagline
            Constraint::Length(1),       // 4 — gap
            Constraint::Length(STEPS_H), // 5 — steps panel
            Constraint::Length(1),       // 6 — gap
            Constraint::Length(3),       // 7 — token input box
            Constraint::Length(1),       // 8 — model selector
            Constraint::Length(1),       // 9 — gap
            Constraint::Length(1),       // 10 — footer
            Constraint::Length(1),       // 11 — status line
            Constraint::Min(0),          // 12 — remaining
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
    let steps_block = Block::default()
        .title(" Get your DeepSeek token ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.bg))
        .style(Style::default().bg(theme.bg));
    let steps_inner = steps_block.inner(chunks[5]);
    frame.render_widget(steps_block, chunks[5]);
    frame.render_widget(Paragraph::new(token_steps(theme)), steps_inner);

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
    let visible_start = if cursor >= box_w {
        cursor - box_w.saturating_sub(1)
    } else {
        0
    };
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
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
                .bg(theme.bg),
            "○",
            "●",
        )
    } else {
        (
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
                .bg(theme.bg),
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
        Span::styled(
            "   (Tab to switch)",
            Style::default().fg(theme.text_dim).bg(theme.bg),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(model_line).alignment(Alignment::Center),
        chunks[8],
    );

    // ── 10. Footer hints ──────────────────────────────────────────
    let sep = |t: &'static str| Span::styled(t, Style::default().fg(theme.text_dim).bg(theme.bg));
    let footer = Line::from(vec![
        key_span("Enter", theme),
        sep(" save · "),
        key_span("Ctrl+O", theme),
        sep(" site · "),
        key_span("Ctrl+Y", theme),
        sep(" copy line · "),
        key_span("Ctrl+C", theme),
        sep(" quit"),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        chunks[10],
    );

    // ── 11. Status line ───────────────────────────────────────────
    let status = ob
        .error
        .map(|msg| (msg, theme.error))
        .or_else(|| ob.info.map(|msg| (msg, theme.success)));
    if let Some((msg, color)) = status {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(color).bg(theme.bg),
            )))
            .alignment(Alignment::Center),
            chunks[11],
        );
    }
}

/// Строк в панели шагов, с рамкой.
const STEPS_H: u16 = 9;

fn key_span(key: &'static str, theme: &Theme) -> Span<'static> {
    Span::styled(
        key,
        Style::default()
            .fg(theme.accent_soft)
            .add_modifier(Modifier::BOLD)
            .bg(theme.bg),
    )
}

/// Шаги получения токена. Сайт не отдаёт токен наружу, поэтому ведём на
/// страницу и даём строку для консоли, которая кладёт его в буфер.
fn token_steps(theme: &Theme) -> Vec<Line<'static>> {
    let number = |n: &'static str| {
        Span::styled(
            n,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
                .bg(theme.bg),
        )
    };
    let text = |t: &'static str| Span::styled(t, Style::default().fg(theme.text_soft).bg(theme.bg));
    let dim = |t: &'static str| Span::styled(t, Style::default().fg(theme.text_dim).bg(theme.bg));
    vec![
        Line::from(vec![
            number(" 1. "),
            key_span("Ctrl+O", theme),
            text(" opens chat.deepseek.com — log in there"),
        ]),
        Line::from(vec![
            number(" 2. "),
            text("Press F12 → Console, paste this line, press Enter:"),
        ]),
        Line::from(vec![
            Span::styled("      ", Style::default().bg(theme.bg)),
            Span::styled(
                TOKEN_CONSOLE_SNIPPET,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
                    .bg(theme.bg),
            ),
        ]),
        Line::from(vec![
            Span::styled("    ", Style::default().bg(theme.bg)),
            key_span("Ctrl+Y", theme),
            dim(" copies it · Chrome may ask you to type: allow pasting"),
        ]),
        Line::from(vec![
            number(" 3. "),
            text("Your token is now in the clipboard — paste it below"),
        ]),
        Line::from(dim(
            "    Or copy userToken from F12 → Application → Local Storage",
        )),
        Line::from(Span::styled(
            " The token is a password: never paste console code from strangers",
            Style::default().fg(theme.warning).bg(theme.bg),
        )),
    ]
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn screen(width: u16, ob: &OnboardingState) -> String {
        let theme = Theme::default_dark();
        let cursor = Cell::new(None);
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal
            .draw(|f| render_onboarding(f, f.area(), ob, 0, &theme, &cursor))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Строку для консоли человек перепечатывает или копирует с экрана —
    /// обрезанная, она молча не сработает.
    #[test]
    fn the_console_line_and_the_helper_keys_fit_on_a_standard_terminal() {
        let text = screen(80, &OnboardingState::default());
        assert!(text.contains(TOKEN_CONSOLE_SNIPPET), "{text}");
        assert!(text.contains("Ctrl+O"), "{text}");
        assert!(text.contains("Ctrl+Y"), "{text}");
        assert!(text.contains("allow pasting"), "{text}");
        assert!(text.contains("Ctrl+C quit"), "{text}");
    }

    /// Узкий и низкий терминал режет текст, но не роняет отрисовку.
    #[test]
    fn a_tiny_terminal_does_not_panic() {
        let theme = Theme::default_dark();
        let ob = OnboardingState {
            input: "token".to_string(),
            cursor: 5,
            ..OnboardingState::default()
        };
        for (width, height) in [(2, 30), (40, 12), (60, 20)] {
            let cursor = Cell::new(None);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| render_onboarding(f, f.area(), &ob, 0, &theme, &cursor))
                .unwrap();
        }
    }

    /// 60 колонок — самый узкий терминал, где строка ещё обязана влезть.
    #[test]
    fn the_console_line_fits_at_sixty_columns() {
        assert!(screen(60, &OnboardingState::default()).contains(TOKEN_CONSOLE_SNIPPET));
    }

    #[test]
    fn a_background_outcome_shows_on_the_status_line() {
        let ob = OnboardingState {
            info: Some("Copied — paste it into the browser console"),
            ..OnboardingState::default()
        };
        assert!(screen(80, &ob).contains("Copied — paste it"));
    }

    #[test]
    #[ignore = "manual: prints the onboarding screen"]
    fn print_onboarding() {
        eprintln!("{}", screen(80, &OnboardingState::default()));
    }
}
