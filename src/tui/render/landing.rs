//! The landing screen shown when the focused conversation has no messages:
//! animated logo, centered input, shortcut hints, and the recent-sessions
//! table.
//!
//! The landing screen animates at a fixed low frame rate while idle, which
//! made its `session::list_sessions` call (reads and JSON-parses every
//! session file) a real per-frame cost; see [`landing_sessions_cached`].

use super::modals::render_autocomplete;
use super::onboarding::pulsing_title;
use super::status::{render_input_border, render_mini_status};
use super::util::{centered_h, fit_col, format_date, provider_label};
use crate::app::AppState;
use crate::config::Config;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// How long the landing screen's session list stays cached before
/// `session::list_sessions` is asked to re-scan the sessions directory. The
/// landing screen only shows up to 5 sessions and repaints at ~8fps while
/// its logo animates, so a few seconds of staleness (new/renamed sessions
/// appearing slightly late) is not user-visible.
const LANDING_SESSIONS_TTL: Duration = Duration::from_secs(3);

thread_local! {
    static LANDING_SESSIONS_CACHE: RefCell<Option<(Instant, Vec<crate::session::SessionSummary>)>> = const { RefCell::new(None) };
}

/// The landing screen's recent-sessions list, refreshed at most once per
/// [`LANDING_SESSIONS_TTL`] instead of on every animation frame.
fn landing_sessions_cached() -> Vec<crate::session::SessionSummary> {
    LANDING_SESSIONS_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((fetched_at, sessions)) = cache.as_ref()
            && fetched_at.elapsed() < LANDING_SESSIONS_TTL
        {
            return sessions.clone();
        }
        let sessions = crate::session::list_sessions().unwrap_or_default();
        *cache = Some((Instant::now(), sessions.clone()));
        sessions
    })
}

pub(super) fn render_landing(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    config: &Config,
    theme: &Theme,
    cursor_cell: &Cell<Option<(u16, u16)>>,
) {
    let input_width = area.width.min(76);
    let x = (area.width - input_width) / 2;

    let sessions = landing_sessions_cached();
    let session_count = sessions.len();
    let show_sessions = session_count > 0;

    // Logo(3) + gap(1) + input(4) + border(1) + shortcuts(1) + gap(1) + sessions(if any: 1+count.min(5)+1) + gap + status(1)
    let center_h = 3
        + 1
        + 4
        + 1
        + 1
        + 1
        + if show_sessions {
            1 + sessions.len().min(5) as u16 + 1
        } else {
            0
        };
    let top_pad = (area.height.saturating_sub(center_h)) / 2;

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(top_pad),
        Constraint::Length(3), // logo block
        Constraint::Length(1), // gap
        Constraint::Length(4), // input
        Constraint::Length(1), // border
        Constraint::Length(1), // shortcuts
        Constraint::Length(1), // gap
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
    let title_spans = pulsing_title(state.focused().generation.animation_tick, theme);
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
    render_input_border(frame, border_area, theme);

    // Shortcuts line
    let info_area = centered_h(chunks[ci], input_width);
    ci += 1;
    let model = format!("{} · {}", provider_label(config), config.active_model());
    let mut shortcut_spans = vec![
        Span::styled(model, Style::default().fg(theme.text_soft).bg(theme.bg)),
        Span::styled("  ", Style::default().bg(theme.bg)),
        Span::styled(
            "Enter ",
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("send  ", Style::default().fg(theme.text_dim)),
        Span::styled(
            "/ ",
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("commands  ", Style::default().fg(theme.text_dim)),
        Span::styled(
            "Esc ",
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("home  ", Style::default().fg(theme.text_dim)),
        Span::styled(
            "Ctrl+C ",
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("quit", Style::default().fg(theme.text_dim)),
    ];
    if state.goal.mode {
        let goal_label = match state.goal.stage {
            crate::app::events::GoalStage::Inactive => "[GOAL ON]".to_string(),
            crate::app::events::GoalStage::WaitForGoal => "[WAITING FOR GOAL]".to_string(),
            crate::app::events::GoalStage::RunAgent1 => {
                format!("[GOAL iter#{}]", state.goal.iteration)
            }
            crate::app::events::GoalStage::RunEvaluator => "[EVALUATING]".to_string(),
            crate::app::events::GoalStage::Done => "[GOAL DONE]".to_string(),
        };
        shortcut_spans.push(Span::styled("  ", Style::default().bg(theme.bg)));
        shortcut_spans.push(Span::styled(
            goal_label,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
                .bg(theme.bg),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![Line::from(shortcut_spans)])
            .alignment(Alignment::Center)
            .style(Style::default().bg(theme.bg)),
        info_area,
    );

    // skip gap
    ci += 1;

    // Sessions table
    if show_sessions {
        let sc = chunks[ci];
        render_sessions_table(frame, sc, &sessions, input_width, theme);
    }

    // Status at very bottom
    let status_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    render_mini_status(frame, status_area, state, config, theme, false);
}

fn render_sessions_table(
    frame: &mut Frame,
    area: Rect,
    sessions: &[crate::session::SessionSummary],
    input_width: u16,
    theme: &Theme,
) {
    let area = centered_h(area, input_width);
    let header_style = Style::default().fg(theme.text_dim).bg(theme.bg);
    let id_style = Style::default().fg(theme.accent_soft).bg(theme.bg);
    let title_style = Style::default().fg(theme.fg).bg(theme.bg);
    let date_style = Style::default().fg(theme.text_dim).bg(theme.bg);
    let model_style = Style::default().fg(theme.accent_soft).bg(theme.bg);
    let sep_style = Style::default().fg(theme.border).bg(theme.bg);
    let gap_style = Style::default().bg(theme.bg);

    // Column budget within the (centered) table width. `id`, `model`, and
    // `date` take fixed slots; `title` flexes to fill whatever they leave, so
    // the row can't overflow the width and clip the right-hand date column.
    // A right pad keeps the row off the last column. Widths are display cells.
    let w = area.width as usize;
    const LEAD: usize = 2;
    const GAP: usize = 2;
    const ID_W: usize = 16;
    const MODEL_W: usize = 16;
    const DATE_W: usize = 12; // "Mon DD HH:MM"
    const RIGHT_PAD: usize = 2;
    let fixed = LEAD + ID_W + GAP + GAP + MODEL_W + GAP + DATE_W + RIGHT_PAD;
    let title_w = w.saturating_sub(fixed).max(8);

    let mut lines: Vec<Line> = Vec::new();

    // Header + a column rule sized to match the row layout below.
    lines.push(Line::from(vec![Span::styled(
        " Recent sessions ",
        header_style,
    )]));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "{}{}{}{}{}{}{}{}",
            " ".repeat(LEAD),
            "─".repeat(ID_W),
            " ".repeat(GAP),
            "─".repeat(title_w),
            " ".repeat(GAP),
            "─".repeat(MODEL_W),
            " ".repeat(GAP),
            "─".repeat(DATE_W),
        ),
        sep_style,
    )]));

    for s in sessions.iter().take(5) {
        let date = format_date(&s.updated_at);
        let model_col = if s.model_type.is_empty() {
            String::new()
        } else {
            format!("[{}]", s.model_type)
        };
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(LEAD), gap_style),
            Span::styled(fit_col(&s.id, ID_W), id_style),
            Span::styled(" ".repeat(GAP), gap_style),
            Span::styled(fit_col(&s.title, title_w), title_style),
            Span::styled(" ".repeat(GAP), gap_style),
            Span::styled(fit_col(&model_col, MODEL_W), model_style),
            Span::styled(" ".repeat(GAP), gap_style),
            Span::styled(fit_col(&date, DATE_W), date_style),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}
